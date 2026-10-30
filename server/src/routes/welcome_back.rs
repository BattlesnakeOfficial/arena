//! `/welcome-back`: where the play.battlesnake.com shutdown banner sends
//! people. One public URL that walks every returning player to their
//! migrated account, however they signed in to play.
//!
//! - Signed out: what comes along, and a GitHub sign-in that lands back here
//!   (the page saves itself as the session's post-login redirect). Signing in
//!   with the GitHub account used on play claims it automatically.
//! - Signed in, already claimed: confirmation and where to find things.
//! - Signed in, nothing claimed: the claim options for email/password and
//!   other-GitHub-account players.

use axum::{extract::State, http::StatusCode, response::IntoResponse};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};

use crate::{
    components::page_factory::PageFactory,
    errors::ServerResult,
    models::{imported_account, session},
    routes::{auth::CurrentSession, claim},
    state::AppState,
};

pub const PATH: &str = "/welcome-back";

/// The day play.battlesnake.com stops serving and redirects here.
const PLAY_SHUTDOWN_DATE: &str = "November 30";

/// GET /welcome-back
pub async fn welcome_back_page(
    State(state): State<AppState>,
    CurrentSession { session, user }: CurrentSession,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let body = match user {
        None => {
            session::set_post_login_redirect(&state.db, session.session_id, PATH.to_string())
                .await
                .wrap_err("Failed to store post-login redirect")?;
            signed_out()
        }
        Some(user) => match imported_account::claimed_username(&state.db, user.user_id).await? {
            Some(username) => claimed(&username),
            None => unclaimed(),
        },
    };

    Ok(page_factory.create_page("Welcome back".to_string(), Box::new(body)))
}

fn signed_out() -> Markup {
    html! {
        div class="home welcome-back" {
            section class="section" {
                h1 { "Battlesnake has a new home" }
                p class="lede" {
                    "Played on play.battlesnake.com? Your snakes, profile, and customization "
                    "unlocks come with you. play.battlesnake.com closes on " (PLAY_SHUTDOWN_DATE) "."
                }
                div class="cta-row" {
                    a class="btn solid" href="/auth/github?resume=true" {
                        "Sign in with GitHub to move your account"
                    }
                }
                p class="fine" {
                    "Signed in to play with an email and password? Sign in here with any GitHub "
                    "account, and this page shows you how to bring your account over."
                }
            }
        }
    }
}

fn claimed(username: &str) -> Markup {
    html! {
        div class="home welcome-back" {
            section class="section" {
                h1 { "You're all set" }
                p class="lede" {
                    "Your play.battlesnake.com account, " strong { (username) } ", is linked. "
                    "Your snakes, profile, and customization unlocks came with you."
                }
                div class="cta-row" {
                    a class="btn solid" href="/battlesnakes" { "Your snakes" }
                    a class="btn" href="/me" { "Your profile" }
                }
            }
        }
    }
}

fn unclaimed() -> Markup {
    html! {
        div class="home welcome-back" {
            section class="section" {
                h1 { "Bring your account over" }
                p class="lede" {
                    "Your snakes, profile, and customization unlocks from play.battlesnake.com "
                    "can come with you before it closes on " (PLAY_SHUTDOWN_DATE) ". "
                    "How depends on how you signed in to play:"
                }
                div class="claim-prompt" {
                    (claim::claim_options())
                    p class="fine" {
                        "New to Battlesnake? There's nothing to claim. "
                        a href="/" { "Head to the home page" }
                        "."
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use sqlx::PgPool;
    use tower::ServiceExt as _;
    use uuid::Uuid;

    use crate::{
        models::session::{self, SESSION_COOKIE_NAME},
        routes::test_support::{create_user_session, session_user_id, signed_session_cookie},
        state::AppState,
    };

    async fn get_page(state: &AppState, session_id: Uuid) -> (StatusCode, String) {
        let app = crate::routes::routes(state.clone())
            .expect("asset router")
            .layer(tower_cookies::CookieManagerLayer::new());
        let response = app
            .oneshot(
                Request::get(super::PATH)
                    .header(
                        header::COOKIE,
                        format!(
                            "{SESSION_COOKIE_NAME}={}",
                            signed_session_cookie(state, session_id)
                        ),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn post_login_redirect(pool: &PgPool, session_id: Uuid) -> Option<String> {
        sqlx::query_scalar("SELECT post_login_redirect FROM sessions WHERE session_id = $1")
            .bind(session_id)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn signed_out_offers_github_sign_in_that_returns_here(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let session = session::create_session(&pool).await.unwrap();

        let (status, page) = get_page(&state, session.session_id).await;

        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("Battlesnake has a new home"));
        assert!(page.contains(r#"href="/auth/github?resume=true""#));
        assert_eq!(
            post_login_redirect(&pool, session.session_id)
                .await
                .as_deref(),
            Some(super::PATH)
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn signed_in_without_a_claim_shows_every_claim_option(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let session_id = create_user_session(&pool, 41001, false).await;

        let (status, page) = get_page(&state, session_id).await;

        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("Bring your account over"));
        for target in [
            r#"href="/auth/github""#,
            r#"href="/claim""#,
            r#"href="/claim/email""#,
        ] {
            assert!(page.contains(target), "welcome-back is missing {target}");
        }
        // The /me prompt's dismiss button has no meaning here.
        assert!(!page.contains("/claim/dismiss-prompt"));
        // Signed-in visits don't touch the post-login redirect.
        assert_eq!(post_login_redirect(&pool, session_id).await, None);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn signed_in_with_a_claim_confirms_the_linked_account(pool: PgPool) {
        let state = AppState::test_from_pool(pool.clone());
        let session_id = create_user_session(&pool, 41002, false).await;
        let user_id = session_user_id(&pool, session_id).await;
        sqlx::query(
            "INSERT INTO imported_accounts
                (play_user_id, play_account_id, email, username, password_hash, is_email_verified,
                 claimed_by_user_id, claimed_at)
             VALUES ('welcome-user', 'welcome-account', 'welcome@example.com', 'old-snake-dev',
                     'unused', true, $1, NOW())",
        )
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

        let (status, page) = get_page(&state, session_id).await;

        assert_eq!(status, StatusCode::OK);
        assert!(page.contains("You&#39;re all set") || page.contains("You're all set"));
        assert!(page.contains("old-snake-dev"));
        assert!(!page.contains(r#"href="/claim""#));
    }
}
