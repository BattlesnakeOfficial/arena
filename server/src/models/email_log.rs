//! Per-recipient email send bookkeeping (BS-7e38).
//!
//! Play's safety net, ported: no matter what triggers a send, one recipient
//! address can only receive so many emails per hour. This is the backstop
//! against both logic bugs (a looping job) and abuse (a user-triggerable
//! flow pointed at someone else's inbox).

use color_eyre::eyre::Context as _;
use sqlx::PgPool;

/// Record an attempted send and return how many attempts have targeted this
/// recipient (case-insensitive) in the trailing hour — including this one.
///
/// Record-before-check, like `claim_attempts`: concurrent sends each insert
/// first and therefore see each other, and a suppressed send still spends
/// budget, so retrying a rate-limited trigger never earns extra email.
pub async fn record_and_count_recent_sends(
    pool: &PgPool,
    recipient: &str,
    purpose: &str,
) -> cja::Result<i64> {
    sqlx::query!(
        "INSERT INTO email_log (recipient, purpose) VALUES ($1, $2)",
        recipient,
        purpose,
    )
    .execute(pool)
    .await
    .wrap_err("Failed to record email send")?;

    let count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) as "count!"
         FROM email_log
         WHERE lower(recipient) = lower($1)
           AND sent_at > NOW() - INTERVAL '1 hour'"#,
        recipient,
    )
    .fetch_one(pool)
    .await
    .wrap_err("Failed to count recent email sends")?;

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../migrations")]
    async fn counts_are_per_recipient_case_insensitive_and_windowed(
        pool: PgPool,
    ) -> cja::Result<()> {
        assert_eq!(
            record_and_count_recent_sends(&pool, "a@example.com", "test").await?,
            1
        );
        // Case variants share a budget.
        assert_eq!(
            record_and_count_recent_sends(&pool, "A@Example.COM", "test").await?,
            2
        );
        // Different recipients don't.
        assert_eq!(
            record_and_count_recent_sends(&pool, "b@example.com", "test").await?,
            1
        );

        // Attempts older than the window slide out.
        sqlx::query!(
            "UPDATE email_log SET sent_at = NOW() - INTERVAL '2 hours'
             WHERE lower(recipient) = 'a@example.com'",
        )
        .execute(&pool)
        .await?;
        assert_eq!(
            record_and_count_recent_sends(&pool, "a@example.com", "test").await?,
            1
        );

        Ok(())
    }
}
