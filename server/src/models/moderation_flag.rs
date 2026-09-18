use color_eyre::eyre::Context as _;
use sqlx::PgPool;
use uuid::Uuid;

/// A recorded moderation decision on a user-submitted free-text field.
/// Rows exist only for `blocked`, `flagged`, and `unchecked` decisions —
/// clean allows write nothing.
#[derive(Debug, Clone)]
pub struct ModerationFlag {
    pub moderation_flag_id: Uuid,
    pub field_kind: String,
    pub text: String,
    pub subject_id: Option<Uuid>,
    pub user_id: Uuid,
    pub decision: String,
    pub hate_or_slur: Option<f64>,
    pub sexual_or_graphic: Option<f64>,
    pub harassment_or_threat: Option<f64>,
    pub impersonates_staff_or_platform: Option<f64>,
    pub disguised_evasion: Option<f64>,
    pub action_choice: Option<String>,
    pub action_confidence: Option<f64>,
    pub action_block_mass: Option<f64>,
    pub model: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub reviewed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub review_outcome: Option<String>,
}

/// Values for one insert. Probabilities optional because `unchecked`
/// rows (and `hardblock` rows) have none.
pub struct NewModerationFlag<'a> {
    pub field_kind: &'a str,
    pub text: &'a str,
    pub subject_id: Option<Uuid>,
    pub user_id: Uuid,
    pub decision: &'a str,
    pub hate_or_slur: Option<f64>,
    pub sexual_or_graphic: Option<f64>,
    pub harassment_or_threat: Option<f64>,
    pub impersonates_staff_or_platform: Option<f64>,
    pub disguised_evasion: Option<f64>,
    pub action_choice: Option<&'a str>,
    pub action_confidence: Option<f64>,
    pub action_block_mass: Option<f64>,
    pub model: Option<&'a str>,
}

/// Best-effort insert of a moderation audit row. Callers treat failures as
/// non-fatal: the moderation decision applies regardless of whether its
/// audit trail landed.
pub async fn insert_flag(pool: &PgPool, flag: &NewModerationFlag<'_>) -> cja::Result<()> {
    sqlx::query!(
        r#"
        INSERT INTO moderation_flags (
            field_kind, text, subject_id, user_id, decision,
            hate_or_slur, sexual_or_graphic, harassment_or_threat,
            impersonates_staff_or_platform, disguised_evasion,
            action_choice, action_confidence, action_block_mass, model
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
        "#,
        flag.field_kind,
        flag.text,
        flag.subject_id,
        flag.user_id,
        flag.decision,
        flag.hate_or_slur,
        flag.sexual_or_graphic,
        flag.harassment_or_threat,
        flag.impersonates_staff_or_platform,
        flag.disguised_evasion,
        flag.action_choice,
        flag.action_confidence,
        flag.action_block_mass,
        flag.model,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to insert moderation flag")?;
    Ok(())
}

/// A flag row joined with the owner's `github_login` for the admin list.
pub struct ModerationFlagListing {
    pub moderation_flag_id: Uuid,
    pub field_kind: String,
    pub text: String,
    pub subject_id: Option<Uuid>,
    pub user_id: Uuid,
    pub user_login: String,
    pub decision: String,
    pub hate_or_slur: Option<f64>,
    pub sexual_or_graphic: Option<f64>,
    pub harassment_or_threat: Option<f64>,
    pub impersonates_staff_or_platform: Option<f64>,
    pub disguised_evasion: Option<f64>,
    pub action_choice: Option<String>,
    pub action_confidence: Option<f64>,
    pub action_block_mass: Option<f64>,
    pub model: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Unreviewed flags, newest first. INNER JOIN: `user_id` is NOT NULL with
/// ON DELETE CASCADE and `users.github_login` is NOT NULL, so an unmatched
/// row is impossible and `user_login` is a plain `String`.
pub async fn list_unreviewed(pool: &PgPool, limit: i64) -> cja::Result<Vec<ModerationFlagListing>> {
    let rows = sqlx::query_as!(
        ModerationFlagListing,
        r#"
        SELECT mf.moderation_flag_id, mf.field_kind, mf.text, mf.subject_id, mf.user_id,
               u.github_login AS user_login, mf.decision,
               mf.hate_or_slur, mf.sexual_or_graphic, mf.harassment_or_threat,
               mf.impersonates_staff_or_platform, mf.disguised_evasion,
               mf.action_choice, mf.action_confidence, mf.action_block_mass,
               mf.model, mf.created_at
        FROM moderation_flags mf
        INNER JOIN users u ON u.user_id = mf.user_id
        WHERE mf.reviewed_at IS NULL
        ORDER BY mf.created_at DESC
        LIMIT $1
        "#,
        limit
    )
    .fetch_all(pool)
    .await
    .wrap_err("Failed to list unreviewed moderation flags")?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_user(pool: &PgPool, github_id: i64) -> cja::Result<Uuid> {
        let row = sqlx::query!(
            "INSERT INTO users (external_github_id, github_login, github_access_token)
             VALUES ($1, $2, 'test-token')
             RETURNING user_id",
            github_id,
            format!("gh-user-{github_id}"),
        )
        .fetch_one(pool)
        .await?;
        Ok(row.user_id)
    }

    fn sample_flag<'a>(user_id: Uuid, text: &'a str, decision: &'a str) -> NewModerationFlag<'a> {
        NewModerationFlag {
            field_kind: "snake_name",
            text,
            subject_id: None,
            user_id,
            decision,
            hate_or_slur: Some(0.11),
            sexual_or_graphic: Some(0.02),
            harassment_or_threat: Some(0.03),
            impersonates_staff_or_platform: Some(0.01),
            disguised_evasion: Some(0.04),
            action_choice: Some("flag_for_review"),
            action_confidence: Some(0.42),
            action_block_mass: Some(0.55),
            model: Some("jev-test"),
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn insert_and_list_round_trips_newest_first(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9301).await?;

        insert_flag(&pool, &sample_flag(user_id, "First", "flagged")).await?;
        // created_at defaults are close together; force ordering with a
        // measurable gap so "newest first" is deterministic.
        sqlx::query!("UPDATE moderation_flags SET created_at = NOW() - INTERVAL '1 hour'")
            .execute(&pool)
            .await?;
        insert_flag(&pool, &sample_flag(user_id, "Second", "blocked")).await?;

        let listings = list_unreviewed(&pool, 200).await?;
        assert_eq!(listings.len(), 2);
        assert_eq!(listings[0].text, "Second");
        assert_eq!(listings[1].text, "First");

        let first = &listings[0];
        assert_eq!(first.field_kind, "snake_name");
        assert_eq!(first.decision, "blocked");
        assert_eq!(first.user_login, "gh-user-9301");
        assert_eq!(first.action_choice.as_deref(), Some("flag_for_review"));
        assert!((first.action_block_mass.unwrap() - 0.55).abs() < 1e-9);
        assert!((first.hate_or_slur.unwrap() - 0.11).abs() < 1e-9);
        assert_eq!(first.model.as_deref(), Some("jev-test"));
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn reviewed_rows_are_excluded(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9302).await?;
        insert_flag(&pool, &sample_flag(user_id, "To review", "flagged")).await?;

        sqlx::query!(
            "UPDATE moderation_flags
             SET reviewed_at = NOW(), review_outcome = 'dismissed'
             WHERE user_id = $1",
            user_id
        )
        .execute(&pool)
        .await?;

        assert!(list_unreviewed(&pool, 200).await?.is_empty());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_inserts_do_not_collide(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9303).await?;
        let a_flag = sample_flag(user_id, "Leg A", "flagged");
        let b_flag = sample_flag(user_id, "Leg B", "flagged");
        let (a, b) = tokio::join!(insert_flag(&pool, &a_flag), insert_flag(&pool, &b_flag),);
        a?;
        b?;

        assert_eq!(list_unreviewed(&pool, 200).await?.len(), 2);
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unchecked_row_tolerates_missing_probabilities(pool: PgPool) -> cja::Result<()> {
        let user_id = create_user(&pool, 9304).await?;
        insert_flag(
            &pool,
            &NewModerationFlag {
                field_kind: "snake_name",
                text: "Judge was down",
                subject_id: None,
                user_id,
                decision: "unchecked",
                hate_or_slur: None,
                sexual_or_graphic: None,
                harassment_or_threat: None,
                impersonates_staff_or_platform: None,
                disguised_evasion: None,
                action_choice: None,
                action_confidence: None,
                action_block_mass: None,
                model: Some("jev-latest"),
            },
        )
        .await?;

        let listings = list_unreviewed(&pool, 200).await?;
        assert_eq!(listings.len(), 1);
        assert_eq!(listings[0].decision, "unchecked");
        assert!(listings[0].hate_or_slur.is_none());
        Ok(())
    }
}
