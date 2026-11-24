//! Paths from play.battlesnake.com, resolved to this site's pages.
//!
//! Old play links are pinned all over the docs blog, Discord, READMEs and
//! search results, and play.battlesnake.com's DNS will eventually point here.
//! This module maps old play URLs to wherever that content lives now
//! (DEV-2210). Redirects are temporary, like `redirects.rs`, so this site stays
//! free to move its own URLs.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
};
use color_eyre::eyre::Context as _;
use maud::html;

use crate::{
    components::page_factory::PageFactory,
    errors::ServerResult,
    models::{imported_account, user},
    state::AppState,
};

/// GET /u/{username} and /profile/{username} (with or without a trailing
/// slash): play's profile URLs, by play username.
///
/// The player's claimed arena account wins, because a play username can
/// differ from the GitHub login arena keys profiles by. Failing that, an arena
/// user whose GitHub login matches (most play accounts were made with GitHub).
/// Otherwise, a 404 page saying they haven't moved over yet.
pub async fn play_profile(
    State(state): State<AppState>,
    Path(username): Path<String>,
    page_factory: PageFactory,
) -> ServerResult<Response, StatusCode> {
    if let Some((login, user_id)) =
        imported_account::claimant_of_play_username(&state.db, &username).await?
    {
        return Ok(Redirect::temporary(&format!("/users/{login}/{user_id}")).into_response());
    }
    if let Some(user) = user::get_user_by_github_login(&state.db, &username)
        .await
        .wrap_err("Failed to look up user for play profile link")?
    {
        return Ok(
            Redirect::temporary(&format!("/users/{}/{}", user.github_login, user.user_id))
                .into_response(),
        );
    }

    Ok((
        StatusCode::NOT_FOUND,
        page_factory.create_page(
            "Player not found".to_string(),
            Box::new(html! {
                div class="home" {
                    section class="section" {
                        h1 { "This player hasn't moved over yet" }
                        p class="empty" {
                            "There's no Battlesnake profile for " strong { (username) } " on this site yet. "
                            "Players from play.battlesnake.com can bring their snakes and profile over whenever they're ready."
                        }
                        div class="cta-row" {
                            a class="btn solid" href="/players" { "Browse players" }
                            a class="btn" href="/welcome-back" { "Move my account" }
                        }
                    }
                }
            }),
        ),
    )
        .into_response())
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

    use crate::{routes::test_support::create_user, state::AppState};

    async fn get(pool: &PgPool, path: &str) -> (StatusCode, Option<String>, String) {
        let state = AppState::test_from_pool(pool.clone());
        let app = crate::routes::routes(state)
            .expect("asset router")
            .into_router();
        let response = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_string());
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, location, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn set_login(pool: &PgPool, user_id: Uuid, login: &str) {
        sqlx::query("UPDATE users SET github_login = $2 WHERE user_id = $1")
            .bind(user_id)
            .bind(login)
            .execute(pool)
            .await
            .unwrap();
    }

    /// An imported play account named `username`, claimed by `claimed_by` if given.
    async fn play_account(pool: &PgPool, username: &str, claimed_by: Option<Uuid>) {
        sqlx::query(
            "INSERT INTO imported_accounts
                (play_user_id, play_account_id, email, username, password_hash, is_email_verified,
                 claimed_by_user_id, claimed_at)
             VALUES ($1, $1, $1 || '@example.com', $1, 'unused', true, $2,
                     CASE WHEN $2::uuid IS NULL THEN NULL ELSE NOW() END)",
        )
        .bind(username)
        .bind(claimed_by)
        .execute(pool)
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claimed_play_username_redirects_to_the_claimants_profile(pool: PgPool) {
        let claimant = create_user(&pool, 52001, false).await;
        set_login(&pool, claimant, "NewGitHubLogin").await;
        play_account(&pool, "OldPlayName", Some(claimant)).await;
        let expected = format!("/users/NewGitHubLogin/{claimant}");

        for path in [
            "/u/OldPlayName",
            "/u/oldplayname/",
            "/profile/OldPlayName",
            "/profile/OLDPLAYNAME/",
        ] {
            let (status, location, _) = get(&pool, path).await;
            assert_eq!(status, StatusCode::TEMPORARY_REDIRECT, "{path}");
            assert_eq!(location.as_deref(), Some(expected.as_str()), "{path}");
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn claim_wins_over_a_different_user_with_that_login(pool: PgPool) {
        let claimant = create_user(&pool, 52002, false).await;
        set_login(&pool, claimant, "claimant-login").await;
        play_account(&pool, "shared-name", Some(claimant)).await;
        let other = create_user(&pool, 52003, false).await;
        set_login(&pool, other, "shared-name").await;

        let (status, location, _) = get(&pool, "/u/shared-name").await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            location.as_deref(),
            Some(format!("/users/claimant-login/{claimant}").as_str())
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn falls_back_to_a_matching_github_login(pool: PgPool) {
        let user_id = create_user(&pool, 52004, false).await;
        set_login(&pool, user_id, "SameEverywhere").await;
        // Their play account exists but isn't claimed; the login still matches.
        play_account(&pool, "sameeverywhere", None).await;

        let (status, location, _) = get(&pool, "/profile/sameeverywhere").await;
        assert_eq!(status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(
            location.as_deref(),
            Some(format!("/users/SameEverywhere/{user_id}").as_str())
        );
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn unmoved_and_unknown_players_get_a_404_page(pool: PgPool) {
        play_account(&pool, "not-moved-yet", None).await;
        for path in ["/u/not-moved-yet", "/profile/nobody-at-all"] {
            let (status, location, body) = get(&pool, path).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
            assert_eq!(location, None);
            assert!(
                body.contains("hasn&#39;t moved over yet")
                    || body.contains("hasn't moved over yet")
            );
            assert!(body.contains(r#"href="/welcome-back""#));
            assert!(body.contains(r#"href="/players""#));
        }
    }
}
