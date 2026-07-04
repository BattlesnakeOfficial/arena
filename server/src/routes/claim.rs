use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use serde::Deserialize;

use crate::{
    components::page_factory::PageFactory, django_password, errors::ServerResult, flasher::Flasher,
    models::imported_account, routes::auth::CurrentUser, state::AppState,
};

/// Failed attempts per user per hour before the claim form locks out.
const MAX_FAILED_ATTEMPTS_PER_HOUR: i64 = 5;

fn claim_form(error: Option<&str>) -> Markup {
    html! {
        div class="container" style="max-width: 480px;" {
            h1 { "Claim your play.battlesnake.com account" }
            p {
                "If you played on the old site, your snakes, profile, and "
                "customization unlocks are waiting. Enter your old play email "
                "and password to bring them over to this account."
            }
            p style="color: #666; font-size: 0.9em;" {
                "Had GitHub connected on play? Your account was linked "
                "automatically when you signed in here — check "
                a href="/battlesnakes" { "your battlesnakes" }
                ". This form is only needed if you signed up on play with "
                "email and password."
            }

            @if let Some(error) = error {
                div class="alert alert-danger" style="color: #b00; margin: 12px 0;" {
                    p { (error) }
                }
            }

            form method="post" action="/claim" {
                div style="margin-bottom: 12px;" {
                    label for="email" { "Play account email" }
                    br;
                    input type="email" id="email" name="email" required
                        style="width: 100%; padding: 8px;";
                }
                div style="margin-bottom: 12px;" {
                    label for="password" { "Play account password" }
                    br;
                    input type="password" id="password" name="password" required
                        style="width: 100%; padding: 8px;";
                }
                p style="color: #666; font-size: 0.85em;" {
                    "Your old password is checked once to prove the account is "
                    "yours and is never stored. Arena sign-in stays GitHub-only."
                }
                button type="submit" class="btn btn-primary" { "Claim account" }
            }

            p style="margin-top: 16px; color: #666; font-size: 0.9em;" {
                "Forgot your play password? Contact us on Discord and we'll "
                "verify you another way."
            }
        }
    }
}

/// GET /claim — the legacy account claim form.
pub async fn claim_page(
    CurrentUser(_user): CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page("Claim Play Account".to_string(), Box::new(claim_form(None))))
}

#[derive(Deserialize)]
pub struct ClaimForm {
    pub email: String,
    pub password: String,
}

/// POST /claim — verify the play password and claim the account.
pub async fn submit_claim(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    page_factory: PageFactory,
    flasher: Flasher,
    Form(form): Form<ClaimForm>,
) -> ServerResult<axum::response::Response, StatusCode> {
    let attempts = imported_account::recent_failed_claim_attempts(&state.db, user.user_id)
        .await
        .wrap_err("Failed to check claim attempts")?;
    if attempts >= MAX_FAILED_ATTEMPTS_PER_HOUR {
        return Ok(page_factory
            .create_page(
                "Claim Play Account".to_string(),
                Box::new(claim_form(Some(
                    "Too many failed attempts. Try again in an hour, or reach \
                     out on Discord for help.",
                ))),
            )
            .into_response());
    }

    let candidates = imported_account::find_unclaimed_by_email(&state.db, &form.email)
        .await
        .wrap_err("Failed to look up imported account")?;

    // Case-variant duplicate emails are possible; the password picks the
    // right one. Wrong email and wrong password get the same error so the
    // form doesn't confirm which emails exist.
    let matched = candidates
        .iter()
        .find(|account| django_password::verify(&form.password, &account.password_hash));

    let Some(account) = matched else {
        imported_account::record_failed_claim_attempt(&state.db, user.user_id)
            .await
            .wrap_err("Failed to record claim attempt")?;
        tracing::info!(
            event_type = "play_claim_failed",
            user_id = %user.user_id,
            "failed play account claim attempt"
        );
        return Ok(page_factory
            .create_page(
                "Claim Play Account".to_string(),
                Box::new(claim_form(Some(
                    "No unclaimed play account matches that email and password.",
                ))),
            )
            .into_response());
    };

    let summary =
        imported_account::claim_account(&state.db, account.imported_account_id, user.user_id)
            .await
            .wrap_err("Failed to claim imported account")?;

    let Some(summary) = summary else {
        // Lost a race with another claim (e.g. auto-link in a parallel
        // session). Treat as failure without burning an attempt.
        return Ok(page_factory
            .create_page(
                "Claim Play Account".to_string(),
                Box::new(claim_form(Some(
                    "That account was just claimed. If that wasn't you, reach \
                     out on Discord.",
                ))),
            )
            .into_response());
    };

    tracing::info!(
        event_type = "play_claim_succeeded",
        user_id = %user.user_id,
        play_username = %summary.username,
        snakes = summary.snakes_created,
        grants = summary.grants_created,
        "play account claimed via password"
    );

    flasher
        .add_flash(format!(
            "Welcome back, {}! Brought over {} snake(s) and {} customization unlock(s).",
            summary.username, summary.snakes_created, summary.grants_created
        ))
        .await?;
    Ok(Redirect::to("/battlesnakes").into_response())
}
