use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::notifications::{
    self, EmailRecipient, NewEmail, NewNotification, NotificationKind, RecordResult,
};

pub struct EmailSettings {
    pub confirmed_email: Option<String>,
    pub pending_email: Option<String>,
}

pub enum ConfirmationRequestResult {
    Requested {
        request_id: Uuid,
        receipt: RecordResult,
    },
    AlreadyPending,
    MatchesConfirmed,
    RateLimited,
    NothingPending,
}

pub async fn settings(pool: &PgPool, user_id: Uuid) -> cja::Result<EmailSettings> {
    let row = sqlx::query!(
        r#"SELECT u.notification_email, r.email AS "pending_email?"
           FROM users u LEFT JOIN notification_email_requests r
             ON r.request_id = u.pending_notification_email_request_id
           WHERE u.user_id = $1"#,
        user_id
    )
    .fetch_one(pool)
    .await?;
    Ok(EmailSettings {
        confirmed_email: row.notification_email,
        pending_email: row.pending_email,
    })
}

pub async fn resolved_recipient(
    connection: &mut PgConnection,
    user_id: Uuid,
) -> cja::Result<Option<String>> {
    let row = sqlx::query_scalar!(
        r#"SELECT COALESCE(NULLIF(btrim(u.notification_email), ''), NULLIF(btrim(u.github_email), ''), ia.email) AS "email?"
           FROM users u
           LEFT JOIN LATERAL (
             SELECT email FROM imported_accounts
             WHERE claimed_by_user_id = u.user_id AND email IS NOT NULL AND btrim(email) <> ''
             ORDER BY is_email_verified DESC, imported_account_id LIMIT 1
           ) ia ON true
           WHERE u.user_id = $1"#,
        user_id
    ).fetch_optional(connection).await?.flatten();
    Ok(row)
}

async fn lock_user(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> cja::Result<(Option<String>, Option<Uuid>)> {
    let row = sqlx::query!("SELECT notification_email, pending_notification_email_request_id FROM users WHERE user_id = $1 FOR NO KEY UPDATE", user_id).fetch_one(&mut **tx).await?;
    Ok((
        row.notification_email,
        row.pending_notification_email_request_id,
    ))
}

async fn enqueue(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    email: &str,
) -> cja::Result<ConfirmationRequestResult> {
    let count = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM notification_email_requests WHERE user_id = $1 AND created_at > NOW() - INTERVAL '24 hours'", user_id).fetch_one(&mut **tx).await?;
    if count >= 5 {
        return Ok(ConfirmationRequestResult::RateLimited);
    }
    let request_id = sqlx::query_scalar!("INSERT INTO notification_email_requests (user_id, email) VALUES ($1, $2) RETURNING request_id", user_id, email).fetch_one(&mut **tx).await?;
    sqlx::query!(
        "UPDATE users SET pending_notification_email_request_id = $2 WHERE user_id = $1",
        user_id,
        request_id
    )
    .execute(&mut **tx)
    .await?;
    let receipt = notifications::record_in_tx(
        tx,
        NewNotification {
            user_id,
            kind: NotificationKind::NotificationEmailConfirmation,
            title: "Confirm notification email".into(),
            body: "Confirm your notification email address to receive optional email updates."
                .into(),
            link: Some("/me".into()),
            email: Some(NewEmail {
                subject: None,
                text: None,
                html_body: None,
                recipient: EmailRecipient::Override(email.into()),
                claim_imported_account_id: None,
                notification_email_request_id: Some(request_id),
            }),
        },
    )
    .await?;
    Ok(ConfirmationRequestResult::Requested {
        request_id,
        receipt,
    })
}

pub async fn request_confirmation(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    email: &str,
) -> cja::Result<ConfirmationRequestResult> {
    let (confirmed, pending_id) = lock_user(tx, user_id).await?;
    if confirmed
        .as_deref()
        .is_some_and(|old| old.eq_ignore_ascii_case(email))
    {
        sqlx::query!(
            "UPDATE users SET pending_notification_email_request_id = NULL WHERE user_id = $1",
            user_id
        )
        .execute(&mut **tx)
        .await?;
        return Ok(ConfirmationRequestResult::MatchesConfirmed);
    }
    if let Some(pending_id) = pending_id {
        let pending = sqlx::query_scalar!(
            "SELECT email FROM notification_email_requests WHERE request_id = $1",
            pending_id
        )
        .fetch_one(&mut **tx)
        .await?;
        if pending.eq_ignore_ascii_case(email) {
            return Ok(ConfirmationRequestResult::AlreadyPending);
        }
    }
    enqueue(tx, user_id, email).await
}

pub async fn resend(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> cja::Result<ConfirmationRequestResult> {
    let (_, pending_id) = lock_user(tx, user_id).await?;
    let Some(pending_id) = pending_id else {
        return Ok(ConfirmationRequestResult::NothingPending);
    };
    let email = sqlx::query_scalar!(
        "SELECT email FROM notification_email_requests WHERE request_id = $1",
        pending_id
    )
    .fetch_one(&mut **tx)
    .await?;
    enqueue(tx, user_id, &email).await
}

pub async fn clear(tx: &mut Transaction<'_, Postgres>, user_id: Uuid) -> cja::Result<()> {
    lock_user(tx, user_id).await?;
    sqlx::query!("UPDATE users SET notification_email = NULL, pending_notification_email_request_id = NULL WHERE user_id = $1", user_id).execute(&mut **tx).await?;
    Ok(())
}

pub fn new_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn hash_secret(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

pub async fn consume(pool: &PgPool, user_id: Uuid, secret: &str) -> cja::Result<bool> {
    if secret.len() != 64 || !secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Ok(false);
    }
    let mut tx = pool.begin().await?;
    let (_, pending_id) = lock_user(&mut tx, user_id).await?;
    let Some(pending_id) = pending_id else {
        return Ok(false);
    };
    let email = sqlx::query_scalar!(
        "UPDATE notification_email_requests SET used_at = NOW() WHERE request_id = $1 AND user_id = $2 AND token_hash = $3 AND used_at IS NULL AND expires_at > NOW() RETURNING email",
        pending_id, user_id, hash_secret(secret)
    ).fetch_optional(&mut *tx).await?;
    let Some(email) = email else {
        return Ok(false);
    };
    sqlx::query!("UPDATE users SET notification_email = $2, pending_notification_email_request_id = NULL WHERE user_id = $1", user_id, email).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn user(pool: &PgPool, n: i64) -> cja::Result<Uuid> {
        Ok(sqlx::query_scalar!("INSERT INTO users (external_github_id, github_login, github_access_token) VALUES ($1, $2, '') RETURNING user_id", n, format!("notification-{n}")).fetch_one(pool).await?)
    }

    async fn save(
        pool: &PgPool,
        user_id: Uuid,
        email: &str,
    ) -> cja::Result<ConfirmationRequestResult> {
        let mut tx = pool.begin().await?;
        let result = request_confirmation(&mut tx, user_id, email).await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn recipient(pool: &PgPool, user_id: Uuid) -> cja::Result<Option<String>> {
        let mut connection = pool.acquire().await?;
        resolved_recipient(&mut connection, user_id).await
    }

    async fn wait_for_user_lock_waiters(pool: &PgPool, expected: i64) -> cja::Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let count = sqlx::query_scalar!("SELECT COUNT(*) AS \"count!\" FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock' AND query LIKE '%FROM users WHERE user_id = $1 FOR NO KEY UPDATE%'").fetch_one(pool).await?;
                if count >= expected { return Ok::<_, color_eyre::Report>(()); }
                tokio::task::yield_now().await;
            }
        }).await??;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn user_lock_allows_foreign_key_inserts(pool: PgPool) -> cja::Result<()> {
        let id = user(&pool, 1674008).await?;
        let mut holder = pool.begin().await?;
        lock_user(&mut holder, id).await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            sqlx::query!(
                "INSERT INTO customization_active_weeks (user_id, week_start) VALUES ($1, date_trunc('week', now())::date)",
                id
            )
            .execute(&pool)
            .await
        })
        .await??;
        holder.commit().await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn pending_never_resolves_and_confirmation_preserves_old_until_consumed(
        pool: PgPool,
    ) -> cja::Result<()> {
        let id = user(&pool, 1674001).await?;
        assert!(recipient(&pool, id).await?.is_none());
        let first = save(&pool, id, "first@example.com").await?;
        assert!(matches!(first, ConfirmationRequestResult::Requested { .. }));
        assert!(recipient(&pool, id).await?.is_none());
        let first_id = settings(&pool, id).await?.pending_email.unwrap();
        assert_eq!(first_id, "first@example.com");
        let secret = new_secret();
        let other_id = user(&pool, 1674005).await?;
        sqlx::query!("UPDATE notification_email_requests SET token_hash = $2, expires_at = NOW() + INTERVAL '24 hours' WHERE user_id = $1", id, hash_secret(&secret)).execute(&pool).await?;
        assert!(!consume(&pool, other_id, &secret).await?);
        assert!(!consume(&pool, id, &"f".repeat(64)).await?);
        assert!(consume(&pool, id, &secret).await?);
        assert!(!consume(&pool, id, &secret).await?);
        assert_eq!(
            recipient(&pool, id).await?.as_deref(),
            Some("first@example.com")
        );
        save(&pool, id, "second@example.com").await?;
        assert_eq!(
            recipient(&pool, id).await?.as_deref(),
            Some("first@example.com")
        );
        assert!(matches!(
            save(&pool, id, "SECOND@example.com").await?,
            ConfirmationRequestResult::AlreadyPending
        ));
        assert!(matches!(
            save(&pool, id, "first@example.com").await?,
            ConfirmationRequestResult::MatchesConfirmed
        ));
        assert!(settings(&pool, id).await?.pending_email.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn fallbacks_skip_blank_claimed_accounts(pool: PgPool) -> cja::Result<()> {
        let id = user(&pool, 1674002).await?;
        sqlx::query!(
            "UPDATE users SET github_email = 'public@example.com' WHERE user_id = $1",
            id
        )
        .execute(&pool)
        .await?;
        sqlx::query!("INSERT INTO imported_accounts (play_user_id, play_account_id, email, username, claimed_by_user_id, is_email_verified) VALUES ('p1','a1',' ','one',$1,true), ('p2','a2','play@example.com','two',$1,false)", id).execute(&pool).await?;
        assert_eq!(
            recipient(&pool, id).await?.as_deref(),
            Some("public@example.com")
        );
        sqlx::query!("UPDATE users SET github_email = ' ' WHERE user_id = $1", id)
            .execute(&pool)
            .await?;
        assert_eq!(
            recipient(&pool, id).await?.as_deref(),
            Some("play@example.com")
        );
        sqlx::query!(
            "UPDATE users SET notification_email = 'confirmed@example.com' WHERE user_id = $1",
            id
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            recipient(&pool, id).await?.as_deref(),
            Some("confirmed@example.com")
        );
        let mut tx = pool.begin().await?;
        clear(&mut tx, id).await?;
        tx.commit().await?;
        assert_eq!(
            recipient(&pool, id).await?.as_deref(),
            Some("play@example.com")
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn rolling_request_budget_and_resend(pool: PgPool) -> cja::Result<()> {
        let id = user(&pool, 1674003).await?;
        let mut tx = pool.begin().await?;
        assert!(matches!(
            resend(&mut tx, id).await?,
            ConfirmationRequestResult::NothingPending
        ));
        tx.commit().await?;
        for n in 0..5 {
            assert!(matches!(
                save(&pool, id, &format!("user{n}@example.com")).await?,
                ConfirmationRequestResult::Requested { .. }
            ));
        }
        assert!(matches!(
            save(&pool, id, "sixth@example.com").await?,
            ConfirmationRequestResult::RateLimited
        ));
        let before = settings(&pool, id).await?.pending_email;
        let mut tx = pool.begin().await?;
        assert!(matches!(
            resend(&mut tx, id).await?,
            ConfirmationRequestResult::RateLimited
        ));
        tx.commit().await?;
        assert_eq!(settings(&pool, id).await?.pending_email, before);
        sqlx::query!("UPDATE notification_email_requests SET created_at = NOW() - INTERVAL '25 hours' WHERE user_id = $1", id).execute(&pool).await?;
        let mut tx = pool.begin().await?;
        assert!(matches!(
            resend(&mut tx, id).await?,
            ConfirmationRequestResult::Requested { .. }
        ));
        tx.commit().await?;
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn expired_and_stale_links_change_nothing(pool: PgPool) -> cja::Result<()> {
        let id = user(&pool, 1674004).await?;
        save(&pool, id, "old@example.com").await?;
        let secret = new_secret();
        sqlx::query!("UPDATE notification_email_requests SET token_hash = $2, expires_at = NOW() - INTERVAL '1 second' WHERE user_id = $1", id, hash_secret(&secret)).execute(&pool).await?;
        assert!(!consume(&pool, id, &secret).await?);
        sqlx::query!("UPDATE notification_email_requests SET expires_at = NOW() + INTERVAL '24 hours' WHERE user_id = $1", id).execute(&pool).await?;
        let mut tx = pool.begin().await?;
        assert!(matches!(
            resend(&mut tx, id).await?,
            ConfirmationRequestResult::Requested { .. }
        ));
        tx.commit().await?;
        assert!(!consume(&pool, id, &secret).await?);
        save(&pool, id, "new@example.com").await?;
        assert!(!consume(&pool, id, &secret).await?);
        assert_eq!(
            settings(&pool, id).await?.pending_email.as_deref(),
            Some("new@example.com")
        );
        assert!(settings(&pool, id).await?.confirmed_email.is_none());
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_requests_share_the_five_per_day_budget(pool: PgPool) -> cja::Result<()> {
        let id = user(&pool, 1674006).await?;
        for n in 0..4 {
            save(&pool, id, &format!("address{n}@example.com")).await?;
        }
        let mut holder = pool.begin().await?;
        sqlx::query!(
            "SELECT user_id FROM users WHERE user_id = $1 FOR UPDATE",
            id
        )
        .fetch_one(&mut *holder)
        .await?;
        let first_pool = pool.clone();
        let first = tokio::spawn(async move { save(&first_pool, id, "fifth@example.com").await });
        wait_for_user_lock_waiters(&pool, 1).await?;
        let second_pool = pool.clone();
        let second = tokio::spawn(async move { save(&second_pool, id, "sixth@example.com").await });
        wait_for_user_lock_waiters(&pool, 2).await?;
        holder.commit().await?;
        let results = [first.await??, second.await??];
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, ConfirmationRequestResult::Requested { .. }))
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, ConfirmationRequestResult::RateLimited))
                .count(),
            1
        );
        assert_eq!(
            sqlx::query_scalar!(
                "SELECT COUNT(*) AS \"count!\" FROM notification_email_requests WHERE user_id = $1",
                id
            )
            .fetch_one(&pool)
            .await?,
            5
        );
        Ok(())
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn concurrent_consumption_succeeds_once(pool: PgPool) -> cja::Result<()> {
        let id = user(&pool, 1674007).await?;
        save(&pool, id, "winner@example.com").await?;
        let secret = new_secret();
        sqlx::query!("UPDATE notification_email_requests SET token_hash = $2, expires_at = NOW() + INTERVAL '24 hours' WHERE user_id = $1", id, hash_secret(&secret)).execute(&pool).await?;
        let mut holder = pool.begin().await?;
        sqlx::query!(
            "SELECT user_id FROM users WHERE user_id = $1 FOR UPDATE",
            id
        )
        .fetch_one(&mut *holder)
        .await?;
        let first_pool = pool.clone();
        let first_secret = secret.clone();
        let first = tokio::spawn(async move { consume(&first_pool, id, &first_secret).await });
        wait_for_user_lock_waiters(&pool, 1).await?;
        let second_pool = pool.clone();
        let second = tokio::spawn(async move { consume(&second_pool, id, &secret).await });
        wait_for_user_lock_waiters(&pool, 2).await?;
        holder.commit().await?;
        assert_eq!(u8::from(first.await??) + u8::from(second.await??), 1);
        assert_eq!(
            settings(&pool, id).await?.confirmed_email.as_deref(),
            Some("winner@example.com")
        );
        Ok(())
    }
}
