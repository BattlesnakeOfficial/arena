use axum::{
    Json,
    extract::State,
    http::{
        StatusCode,
        header::{CACHE_CONTROL, HeaderName},
    },
};

use crate::state::AppState;

/// Upper bound on the readiness probe, covering pool acquisition *and* query.
pub const DATABASE_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const GIT_SHA: &str = env!("VERGEN_GIT_SHA");

#[derive(Debug, serde::Serialize)]
pub struct HealthResponse {
    /// `"ok"` iff `database_reachable`.
    pub status: &'static str,
    /// Always `"alive"`: a process that cannot serve produces no response at all.
    pub liveness: &'static str,
    pub version: &'static str,
    pub git_sha: &'static str,
    pub database_reachable: bool,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

/// `GET`/`HEAD /health` — unauthenticated, cheap, no template rendering.
pub async fn health(
    State(state): State<AppState>,
) -> (
    StatusCode,
    [(HeaderName, &'static str); 1],
    Json<HealthResponse>,
) {
    let database_reachable = database_reachable(&state).await;
    let status_code = if database_reachable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status_code,
        [(CACHE_CONTROL, "no-store")],
        Json(HealthResponse {
            status: if database_reachable {
                "ok"
            } else {
                "unavailable"
            },
            liveness: "alive",
            version: SERVER_VERSION,
            git_sha: GIT_SHA,
            database_reachable,
            timestamp: chrono::Utc::now(),
        }),
    )
}

async fn database_reachable(state: &AppState) -> bool {
    // Timeout wraps the WHOLE future (acquisition included): sqlx's default
    // 30s acquire_timeout would otherwise let a saturated pool hang the
    // request far past our bound. Future is dropped (cancelled) on timeout.
    let probe = sqlx::query_scalar!("SELECT 1").fetch_one(&state.db);
    match tokio::time::timeout(DATABASE_CHECK_TIMEOUT, probe).await {
        Ok(Ok(_)) => true,
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "health check database probe failed");
            false
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = DATABASE_CHECK_TIMEOUT.as_secs(),
                "health check database probe timed out"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    #[sqlx::test(migrations = "../migrations")]
    async fn health_returns_200_with_live_db(pool: sqlx::PgPool) {
        let state = crate::state::AppState::test_from_pool(pool);
        let response = health(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("reading health response body");
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("health response is valid JSON");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["database_reachable"], true);
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["git_sha"], env!("VERGEN_GIT_SHA"));
    }

    #[tokio::test]
    async fn health_returns_503_when_db_unreachable() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(2))
            .connect_lazy("postgresql://localhost:1/arena")
            .expect("lazy pool does not connect eagerly");
        let state = crate::state::AppState::test_from_pool(pool);
        let response = health(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        // body still parses and reports the outage
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("reading health response body");
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("health response is valid JSON");
        assert_eq!(json["status"], "unavailable");
        assert_eq!(json["database_reachable"], false);
    }
}
