use std::{collections::HashMap, sync::Mutex, time::Duration};

use chrono::{NaiveDate, Utc};
use sqlx::PgPool;
use uuid::Uuid;

/// One attempted activity write per user and UTC day on this instance.
#[derive(Default)]
pub struct ActivityRecorder {
    attempted: Mutex<HashMap<Uuid, NaiveDate>>,
}

impl ActivityRecorder {
    pub async fn record(&self, db: &PgPool, user_id: Uuid) -> bool {
        self.record_on(db, user_id, Utc::now().date_naive()).await
    }

    async fn record_on(&self, db: &PgPool, user_id: Uuid, day: NaiveDate) -> bool {
        {
            let mut attempted = self.attempted.lock().unwrap_or_else(|e| e.into_inner());
            if attempted.get(&user_id).is_some_and(|last| *last >= day) {
                return false;
            }
            attempted.insert(user_id, day);
        }

        let write = sqlx::query!(
            "INSERT INTO user_activity_days (user_id, day) VALUES ($1, $2) ON CONFLICT (user_id, day) DO NOTHING",
            user_id,
            day
        )
        .execute(db);
        match tokio::time::timeout(Duration::from_secs(1), write).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(%user_id, %day, %error, "Failed to record account activity");
            }
            Err(error) => {
                tracing::warn!(%user_id, %day, %error, "Timed out recording account activity");
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[sqlx::test(migrations = "../migrations")]
    async fn records_once_per_day_and_preserves_reservation_after_failure(db: PgPool) {
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token) VALUES (1, 'active', '') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        let recorder = ActivityRecorder::default();
        let day = Utc::now().date_naive();
        assert!(recorder.record_on(&db, user_id, day).await);
        assert!(!recorder.record_on(&db, user_id, day).await);
        assert!(
            !recorder
                .record_on(&db, user_id, day.pred_opt().unwrap())
                .await
        );
        assert!(
            recorder
                .record_on(&db, user_id, day.succ_opt().unwrap())
                .await
        );
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!: i64\" FROM user_activity_days WHERE user_id = $1",
            user_id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(count, 2);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn stalled_insert_times_out_without_retrying(db: PgPool) {
        let user_id = sqlx::query_scalar!(
            "INSERT INTO users(external_github_id, github_login, github_access_token) VALUES (1, 'timed-out', '') RETURNING user_id"
        ).fetch_one(&db).await.unwrap();
        let mut lock = db.begin().await.unwrap();
        sqlx::query!("LOCK TABLE user_activity_days IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock)
            .await
            .unwrap();
        let recorder = ActivityRecorder::default();
        let start = std::time::Instant::now();
        assert!(recorder.record(&db, user_id).await);
        assert!(start.elapsed() < Duration::from_millis(1_500));
        lock.rollback().await.unwrap();
        assert!(!recorder.record(&db, user_id).await);
        let count = sqlx::query_scalar!(
            "SELECT COUNT(*) AS \"count!: i64\" FROM user_activity_days WHERE user_id = $1",
            user_id
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(count, 0);
    }
}
