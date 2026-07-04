//! Importer that copies play's Postgres into arena's staging tables
//! (`imported_accounts` / `imported_snakes` / `imported_grants`).
//!
//! Run via `arena import-play` with PLAY_DATABASE_URL set (read-only play
//! credentials) alongside the usual DATABASE_URL. Idempotent: re-running
//! refreshes play-side data without touching claim state, so it can run
//! repeatedly during the transition window.
//!
//! Play reads use runtime queries (not sqlx macros) — the play schema
//! isn't part of arena's compile-time database.

use color_eyre::eyre::Context as _;
use sqlx::{PgPool, Row as _};

use crate::models::imported_account::{self, StageAccount, StageSnake};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImportCounts {
    pub accounts: u64,
    pub snakes: u64,
    pub snakes_orphaned: u64,
    pub grants: u64,
    pub grants_orphaned: u64,
}

pub async fn import_from_play(play: &PgPool, arena: &PgPool) -> cja::Result<ImportCounts> {
    let mut counts = ImportCounts::default();

    // One row per play user: identity + profile + optional GitHub link.
    // uid/extra_data are cast to text so this works against both older
    // (text) and newer (jsonb) social_django schemas.
    let account_rows = sqlx::query(
        r#"
        SELECT
            u.id AS play_user_id,
            u.email,
            u.password,
            u.is_email_verified,
            (u.is_staff OR u.is_superuser) AS is_staff,
            a.id AS play_account_id,
            a.username,
            a.display_name,
            COALESCE(a.pronouns, '') AS pronouns,
            a.country,
            a.backstory,
            a.github_username,
            a.points,
            a.points_high_score,
            a.created AS play_created_at,
            s.uid::text AS github_uid_text,
            s.extra_data::text AS github_extra_data
        FROM authentication_user u
        JOIN core_account a ON a.user_id = u.id
        LEFT JOIN social_django_usersocialauth s
            ON s.user_id = u.id AND s.provider = 'github'
        ORDER BY u.id
        "#,
    )
    .fetch_all(play)
    .await
    .wrap_err("Failed to read play accounts")?;

    for row in account_rows {
        let play_user_id: String = row.try_get("play_user_id")?;

        let github_uid = row
            .try_get::<Option<String>, _>("github_uid_text")?
            .and_then(|uid| match uid.parse::<i64>() {
                Ok(uid) => Some(uid),
                Err(_) => {
                    tracing::warn!(
                        play_user_id = %play_user_id,
                        uid = %uid,
                        "Non-numeric GitHub uid in play social auth; treating as unlinked"
                    );
                    None
                }
            });

        // Prefer the login recorded in the OAuth link's extra_data; fall
        // back to the denormalized core_account.github_username.
        let social_login = row
            .try_get::<Option<String>, _>("github_extra_data")?
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| v.get("login").and_then(|l| l.as_str()).map(String::from));
        let github_login = social_login
            .or_else(|| {
                let denormalized: String = row.try_get("github_username").ok()?;
                (!denormalized.is_empty()).then_some(denormalized)
            })
            .filter(|login| !login.is_empty());

        let account = StageAccount {
            play_user_id,
            play_account_id: row.try_get("play_account_id")?,
            email: row.try_get("email")?,
            password_hash: row.try_get("password")?,
            is_email_verified: row.try_get("is_email_verified")?,
            username: row.try_get("username")?,
            display_name: row.try_get("display_name")?,
            pronouns: row.try_get("pronouns")?,
            country: row.try_get("country")?,
            backstory: row.try_get("backstory")?,
            github_uid,
            github_login,
            points: row.try_get("points")?,
            points_high_score: row.try_get("points_high_score")?,
            is_staff: row.try_get("is_staff")?,
            play_created_at: row.try_get("play_created_at")?,
        };

        imported_account::stage_account(arena, &account).await?;
        counts.accounts += 1;
    }

    // Active (non-archived) snakes with an owner.
    let snake_rows = sqlx::query(
        r#"
        SELECT s.id AS play_snake_id, s.account_id, s.name, s.url,
               s.head, s.tail, s.color, s.is_public
        FROM core_snake s
        WHERE s.is_archived = false AND s.account_id IS NOT NULL
        ORDER BY s.id
        "#,
    )
    .fetch_all(play)
    .await
    .wrap_err("Failed to read play snakes")?;

    for row in snake_rows {
        let snake = StageSnake {
            play_snake_id: row.try_get("play_snake_id")?,
            play_account_id: row.try_get("account_id")?,
            name: row.try_get("name")?,
            url: row.try_get("url")?,
            head: row.try_get("head")?,
            tail: row.try_get("tail")?,
            color: row.try_get("color")?,
            is_public: row.try_get("is_public")?,
        };

        if imported_account::stage_snake(arena, &snake).await? {
            counts.snakes += 1;
        } else {
            tracing::warn!(
                play_snake_id = %snake.play_snake_id,
                play_account_id = %snake.play_account_id,
                "Snake owner not staged; skipping"
            );
            counts.snakes_orphaned += 1;
        }
    }

    // Head/tail customization grants, staged by (type, slug).
    let grant_rows = sqlx::query(
        r#"
        SELECT g.account_id, c.customization_type, c.slug
        FROM core_snakecustomizationgrant g
        JOIN core_snakecustomization c ON c.id = g.snake_customization_id
        WHERE c.customization_type IN ('head', 'tail')
        ORDER BY g.id
        "#,
    )
    .fetch_all(play)
    .await
    .wrap_err("Failed to read play grants")?;

    for row in grant_rows {
        let play_account_id: String = row.try_get("account_id")?;
        let customization_type: String = row.try_get("customization_type")?;
        let slug: String = row.try_get("slug")?;

        if imported_account::stage_grant(arena, &play_account_id, &customization_type, &slug)
            .await?
        {
            counts.grants += 1;
        } else {
            counts.grants_orphaned += 1;
        }
    }

    Ok(counts)
}

/// Entry point for the `arena import-play` subcommand.
pub async fn run_import() -> cja::Result<()> {
    let play_url = std::env::var("PLAY_DATABASE_URL")
        .wrap_err("PLAY_DATABASE_URL must be set (read-only play Postgres credentials)")?;
    let arena_url =
        std::env::var("DATABASE_URL").wrap_err("DATABASE_URL must be set (arena Postgres)")?;

    let play = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&play_url)
        .await
        .wrap_err("Failed to connect to play database")?;
    let arena = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&arena_url)
        .await
        .wrap_err("Failed to connect to arena database")?;

    sqlx::migrate!("../migrations")
        .run(&arena)
        .await
        .wrap_err("Failed to run arena migrations")?;

    let counts = import_from_play(&play, &arena).await?;

    println!(
        "Imported {} accounts, {} snakes ({} orphaned), {} grants ({} orphaned)",
        counts.accounts,
        counts.snakes,
        counts.snakes_orphaned,
        counts.grants,
        counts.grants_orphaned
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal play-shaped tables (just the columns the importer reads),
    /// created in the arena test database so play-pool == arena-pool in
    /// tests. Column types match play's real schema.
    async fn create_play_tables(pool: &PgPool) -> cja::Result<()> {
        sqlx::raw_sql(
            r#"
            CREATE TABLE authentication_user (
                id TEXT PRIMARY KEY,
                email TEXT NOT NULL,
                password TEXT NOT NULL,
                is_email_verified BOOLEAN NOT NULL DEFAULT false,
                is_staff BOOLEAN NOT NULL DEFAULT false,
                is_superuser BOOLEAN NOT NULL DEFAULT false
            );
            CREATE TABLE core_account (
                id TEXT PRIMARY KEY,
                user_id TEXT NOT NULL,
                username TEXT NOT NULL,
                display_name TEXT NOT NULL DEFAULT '',
                pronouns TEXT,
                country TEXT NOT NULL DEFAULT '',
                backstory TEXT NOT NULL DEFAULT '',
                github_username TEXT NOT NULL DEFAULT '',
                points INTEGER NOT NULL DEFAULT 0,
                points_high_score INTEGER NOT NULL DEFAULT 0,
                created TIMESTAMPTZ NOT NULL DEFAULT NOW()
            );
            CREATE TABLE social_django_usersocialauth (
                id SERIAL PRIMARY KEY,
                user_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                uid TEXT NOT NULL,
                extra_data JSONB
            );
            CREATE TABLE core_snake (
                id TEXT PRIMARY KEY,
                account_id TEXT,
                name TEXT NOT NULL,
                url TEXT NOT NULL DEFAULT '',
                head TEXT NOT NULL DEFAULT 'default',
                tail TEXT NOT NULL DEFAULT 'default',
                color TEXT NOT NULL DEFAULT '#888888',
                is_public BOOLEAN NOT NULL DEFAULT false,
                is_archived BOOLEAN NOT NULL DEFAULT false
            );
            CREATE TABLE core_snakecustomization (
                id TEXT PRIMARY KEY,
                customization_type TEXT NOT NULL,
                slug TEXT NOT NULL
            );
            CREATE TABLE core_snakecustomizationgrant (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                snake_customization_id TEXT NOT NULL
            );
            "#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn seed_play_data(pool: &PgPool) -> cja::Result<()> {
        sqlx::raw_sql(
            r#"
            INSERT INTO authentication_user (id, email, password, is_email_verified, is_staff, is_superuser) VALUES
                ('usr_linked', 'linked@example.com', 'pbkdf2_sha256$260000$s$h', true, false, false),
                ('usr_legacy', 'legacy@example.com', 'pbkdf2_sha256$260000$s$h', true, false, true),
                ('usr_badsocial', 'bad@example.com', '!unusablepasswordsentinel', false, false, false);

            INSERT INTO core_account (id, user_id, username, display_name, pronouns, country, backstory, github_username, points, points_high_score) VALUES
                ('act_linked', 'usr_linked', 'linkedplayer', 'Linked Player', 'they/them', 'CA', 'story', 'fallback-login', 120, 300),
                ('act_legacy', 'usr_legacy', 'legacyplayer', '', NULL, '', '', '', 0, 50),
                ('act_badsocial', 'usr_badsocial', 'badsocial', 'Bad Social', NULL, '', '', '', 0, 0);

            INSERT INTO social_django_usersocialauth (user_id, provider, uid, extra_data) VALUES
                ('usr_linked', 'github', '777001', '{"login": "linked-gh"}'),
                ('usr_linked', 'twitter', '999', '{}'),
                ('usr_badsocial', 'github', 'not-a-number', '{}');

            INSERT INTO core_snake (id, account_id, name, url, head, tail, color, is_public, is_archived) VALUES
                ('snk_1', 'act_linked', 'Alpha', 'https://example.com/a', 'beluga', 'default', '#ff0000', true, false),
                ('snk_2', 'act_linked', 'Archived', 'https://example.com/b', 'default', 'default', '#00ff00', false, true),
                ('snk_3', 'act_legacy', 'Legacy Snake', 'https://example.com/c', 'default', 'default', '#0000ff', false, false),
                ('snk_4', NULL, 'Orphan', 'https://example.com/d', 'default', 'default', '#888888', false, false),
                ('snk_5', 'act_gone', 'GoneOwner', 'https://example.com/e', 'default', 'default', '#888888', false, false);

            INSERT INTO core_snakecustomization (id, customization_type, slug) VALUES
                ('scst_head', 'head', 'alligator'),
                ('scst_tail', 'tail', 'alligator'),
                ('scst_color', 'color', 'some-color');

            INSERT INTO core_snakecustomizationgrant (id, account_id, snake_customization_id) VALUES
                ('scg_1', 'act_linked', 'scst_head'),
                ('scg_2', 'act_linked', 'scst_tail'),
                ('scg_3', 'act_linked', 'scst_color'),
                ('scg_4', 'act_gone', 'scst_head');
            "#,
        )
        .execute(pool)
        .await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn import_stages_accounts_snakes_and_grants(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        seed_play_data(&pool).await?;

        let counts = import_from_play(&pool, &pool).await?;
        assert_eq!(counts.accounts, 3);
        // Archived and NULL-owner snakes are filtered in SQL; the snake
        // whose owner isn't staged is counted as orphaned.
        assert_eq!(counts.snakes, 2);
        assert_eq!(counts.snakes_orphaned, 1);
        // Color grants are filtered in SQL; the grant with a missing owner
        // is orphaned.
        assert_eq!(counts.grants, 2);
        assert_eq!(counts.grants_orphaned, 1);

        let linked = sqlx::query!(
            r#"SELECT email, username, display_name, pronouns, github_uid,
                      github_login, points, is_staff, is_email_verified
               FROM imported_accounts WHERE play_user_id = 'usr_linked'"#
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(linked.email, "linked@example.com");
        assert_eq!(linked.username, "linkedplayer");
        assert_eq!(linked.pronouns, "they/them");
        assert_eq!(linked.github_uid, Some(777001));
        // extra_data login wins over the denormalized github_username.
        assert_eq!(linked.github_login.as_deref(), Some("linked-gh"));
        assert_eq!(linked.points, 120);
        assert!(!linked.is_staff);
        assert!(linked.is_email_verified);

        // Superuser flag folds into is_staff; NULL pronouns coalesce.
        let legacy = sqlx::query!(
            "SELECT is_staff, pronouns, github_uid FROM imported_accounts
             WHERE play_user_id = 'usr_legacy'"
        )
        .fetch_one(&pool)
        .await?;
        assert!(legacy.is_staff);
        assert_eq!(legacy.pronouns, "");
        assert_eq!(legacy.github_uid, None);

        // Non-numeric social uid degrades to unlinked, import continues.
        let bad = sqlx::query!(
            "SELECT github_uid, github_login FROM imported_accounts
             WHERE play_user_id = 'usr_badsocial'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(bad.github_uid, None);

        let snake_names: Vec<String> =
            sqlx::query!("SELECT name FROM imported_snakes ORDER BY name")
                .fetch_all(&pool)
                .await?
                .into_iter()
                .map(|r| r.name)
                .collect();
        assert_eq!(
            snake_names,
            vec!["Alpha".to_string(), "Legacy Snake".to_string()]
        );

        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn import_is_idempotent(pool: PgPool) -> cja::Result<()> {
        create_play_tables(&pool).await?;
        seed_play_data(&pool).await?;

        import_from_play(&pool, &pool).await?;
        // Simulate play-side changes between sync runs.
        sqlx::raw_sql(
            "UPDATE core_account SET display_name = 'Renamed Player' WHERE id = 'act_linked'",
        )
        .execute(&pool)
        .await?;

        let counts = import_from_play(&pool, &pool).await?;
        assert_eq!(counts.accounts, 3);

        let total = sqlx::query!(r#"SELECT COUNT(*) as "count!" FROM imported_accounts"#)
            .fetch_one(&pool)
            .await?
            .count;
        assert_eq!(total, 3);

        let renamed = sqlx::query!(
            "SELECT display_name FROM imported_accounts WHERE play_user_id = 'usr_linked'"
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(renamed.display_name, "Renamed Player");

        let grant_total = sqlx::query!(r#"SELECT COUNT(*) as "count!" FROM imported_grants"#)
            .fetch_one(&pool)
            .await?
            .count;
        assert_eq!(grant_total, 2);

        Ok(())
    }
}
