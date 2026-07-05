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
    components::page_factory::PageFactory,
    django_password,
    errors::ServerResult,
    flasher::Flasher,
    models::{claim_email_token, imported_account},
    routes::auth::CurrentUser,
    state::AppState,
};

/// Attempts per arena user per hour: stops one account from enumerating
/// many play emails.
const MAX_ATTEMPTS_PER_USER_PER_HOUR: i64 = 5;

/// Attempts per target play email per hour, across ALL arena users: the
/// real brute-force cap, since arena login is GitHub-only and an attacker
/// could otherwise mint a fresh per-user budget per throwaway account. A
/// legitimate owner needs one correct attempt, so this only bites abuse.
///
/// Accepted tradeoff: a cross-user per-email cap means an attacker can burn
/// this budget to lock a known play email out of *password* claim for an
/// hour. That's bounded and has escape hatches — GitHub auto-link (the
/// primary path) is unaffected, and the owner can retry after the window or
/// reach out on Discord. The alternative (per-user only) is the exact hole
/// this closes, so the DoS is the lesser evil. Revisit if griefing shows up.
const MAX_ATTEMPTS_PER_EMAIL_PER_HOUR: i64 = 10;

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
                "Forgot your play password? "
                a href="/claim/email" { "Claim by email instead" }
                " — we'll send a one-time link to your old play address. Or "
                "contact us on Discord and we'll verify you another way."
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
    // Record this attempt before doing any work, then read the counts —
    // that ordering is what makes the rate limit hold under concurrent
    // requests. Both the per-user and per-email windows must be under
    // budget.
    let counts =
        imported_account::record_and_count_claim_attempts(&state.db, user.user_id, &form.email)
            .await
            .wrap_err("Failed to record claim attempt")?;
    if counts.by_user > MAX_ATTEMPTS_PER_USER_PER_HOUR
        || counts.by_email > MAX_ATTEMPTS_PER_EMAIL_PER_HOUR
    {
        tracing::warn!(
            event_type = "play_claim_rate_limited",
            user_id = %user.user_id,
            by_user = counts.by_user,
            by_email = counts.by_email,
            "play account claim rate limited"
        );
        return Ok(page_factory
            .create_page(
                "Claim Play Account".to_string(),
                Box::new(claim_form(Some(
                    "Too many attempts. Try again in an hour, or reach out on \
                     Discord for help.",
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
        // No candidate matched. If the email was unknown we did zero PBKDF2
        // work above; spend one now so a non-existent email costs the same
        // as a wrong password and can't be told apart by response time.
        // Decoy against a real imported hash so its iteration count (and
        // thus timing) matches a genuine account rather than a guessed one.
        if candidates.is_empty() {
            let decoy = imported_account::representative_password_hash(&state.db)
                .await
                .wrap_err("Failed to fetch decoy hash")?
                .unwrap_or_else(|| django_password::FALLBACK_DECOY_HASH.to_string());
            django_password::spend_decoy_work(&form.password, &decoy);
        }
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

    // Notify the play email that the account was claimed — a security signal
    // for the real owner if a claim wasn't theirs. Best-effort.
    state.mailer.notify_account_claimed(
        &state.db,
        state.config.email_per_recipient_hourly_limit,
        &summary,
    );
    state
        .discord
        .notify_account_claimed(&summary.username, summary.snakes_created);

    flasher
        .add_flash(format!(
            "Welcome back, {}! Brought over {} snake(s) and {} customization unlock(s).",
            summary.username, summary.snakes_created, summary.grants_created
        ))
        .await?;
    Ok(Redirect::to("/battlesnakes").into_response())
}

// ---------------------------------------------------------------------------
// Email-recovery claim (BS-7e38): the last-resort path for play users with
// no usable password (OAuth-only play accounts) and no GitHub link. A
// logged-in arena user enters their old play email; if it matches an
// unclaimed imported account we mail that address a one-time link, and
// completing the link (as the same arena user) runs the normal claim.
// ---------------------------------------------------------------------------

fn email_claim_form(notice: Option<Markup>) -> Markup {
    html! {
        div class="container" style="max-width: 480px;" {
            h1 { "Claim by email" }
            p {
                "No usable play password (signed up with GitHub on play, or "
                "just forgot it)? Enter your old play email and we'll send a "
                "one-time link there. Opening it while signed in here "
                "finishes the claim."
            }

            @if let Some(notice) = notice {
                (notice)
            }

            form method="post" action="/claim/email" {
                div style="margin-bottom: 12px;" {
                    label for="email" { "Play account email" }
                    br;
                    input type="email" id="email" name="email" required
                        style="width: 100%; padding: 8px;";
                }
                button type="submit" class="btn btn-primary" { "Email me a claim link" }
            }

            p style="margin-top: 16px; color: #666; font-size: 0.9em;" {
                "Know your play password? "
                a href="/claim" { "Claim with it directly" }
                "."
            }
        }
    }
}

/// The one response POST /claim/email ever gives about the lookup, so the
/// form can't be used to probe which play emails exist.
fn email_claim_sent_notice() -> Markup {
    html! {
        div class="alert alert-success" style="margin: 12px 0;" {
            p {
                "If that address matches an unclaimed play account, a claim "
                "link is on its way. It works once, expires in 30 minutes, "
                "and only completes on this arena account."
            }
        }
    }
}

/// GET /claim/email — request form for the recovery link.
pub async fn email_claim_page(
    CurrentUser(_user): CurrentUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page(
        "Claim by Email".to_string(),
        Box::new(email_claim_form(None)),
    ))
}

#[derive(Deserialize)]
pub struct EmailClaimForm {
    pub email: String,
}

/// POST /claim/email — maybe send a one-time claim link.
pub async fn submit_email_claim(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    page_factory: PageFactory,
    Form(form): Form<EmailClaimForm>,
) -> ServerResult<axum::response::Response, StatusCode> {
    // Shares the claim_attempts budget with the password form: both flows
    // probe the same "which emails exist" surface, so they spend the same
    // per-user and per-email allowances. Record-before-check, as ever.
    let counts =
        imported_account::record_and_count_claim_attempts(&state.db, user.user_id, &form.email)
            .await
            .wrap_err("Failed to record claim attempt")?;
    if counts.by_user > MAX_ATTEMPTS_PER_USER_PER_HOUR
        || counts.by_email > MAX_ATTEMPTS_PER_EMAIL_PER_HOUR
    {
        tracing::warn!(
            event_type = "email_claim_rate_limited",
            user_id = %user.user_id,
            by_user = counts.by_user,
            by_email = counts.by_email,
            "email claim rate limited"
        );
        return Ok(page_factory
            .create_page(
                "Claim by Email".to_string(),
                Box::new(email_claim_form(Some(html! {
                    div class="alert alert-danger" style="color: #b00; margin: 12px 0;" {
                        p {
                            "Too many attempts. Try again in an hour, or reach \
                             out on Discord for help."
                        }
                    }
                }))),
            )
            .into_response());
    }

    let candidates = imported_account::find_unclaimed_by_email(&state.db, &form.email)
        .await
        .wrap_err("Failed to look up imported account")?;

    // Case-variant duplicates are possible; with no password to
    // disambiguate, prefer an account whose email play had verified.
    let target = candidates
        .iter()
        .find(|a| a.is_email_verified)
        .or_else(|| candidates.first());

    if let Some(account) = target {
        let secret =
            claim_email_token::create(&state.db, account.imported_account_id, user.user_id)
                .await
                .wrap_err("Failed to create claim token")?;

        let verify_url = format!(
            "{}/claim/email/verify?token={}",
            state.config.base_url, secret
        );

        // Fire-and-forget, like every other notification — and load-bearing
        // here: awaiting the Mailgun call would make a matching email
        // measurably slower than a miss (and turn a transport error into a
        // 500 only matches can produce), handing back exactly the
        // enumeration oracle the uniform response below exists to prevent.
        state.mailer.notify_claim_verification(
            &state.db,
            state.config.email_per_recipient_hourly_limit,
            &account.email,
            &account.username,
            &verify_url,
        );

        tracing::info!(
            event_type = "email_claim_link_requested",
            user_id = %user.user_id,
            "email claim link requested"
        );
    } else {
        tracing::info!(
            event_type = "email_claim_no_match",
            user_id = %user.user_id,
            "email claim requested for unknown or claimed email"
        );
    }

    // Uniform response whether or not anything matched or sent.
    Ok(page_factory
        .create_page(
            "Claim by Email".to_string(),
            Box::new(email_claim_form(Some(email_claim_sent_notice()))),
        )
        .into_response())
}

#[derive(Deserialize)]
pub struct VerifyQuery {
    pub token: String,
}

/// GET /claim/email/verify?token=… — confirmation page. The claim itself
/// happens on the POST below, so a link prefetched by a mail scanner (or
/// opened twice) consumes nothing.
pub async fn email_claim_verify_page(
    CurrentUser(_user): CurrentUser,
    page_factory: PageFactory,
    axum::extract::Query(query): axum::extract::Query<VerifyQuery>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(page_factory.create_page(
        "Finish Claiming".to_string(),
        Box::new(html! {
            div class="container" style="max-width: 480px;" {
                h1 { "Finish claiming your play account" }
                p {
                    "You're about to attach the play account from your claim "
                    "email to this arena login, bringing its snakes and "
                    "customization unlocks with it."
                }
                form method="post" action="/claim/email/verify" {
                    input type="hidden" name="token" value=(query.token);
                    button type="submit" class="btn btn-primary" { "Complete claim" }
                }
            }
        }),
    ))
}

#[derive(Deserialize)]
pub struct VerifyForm {
    pub token: String,
}

/// POST /claim/email/verify — redeem the one-time token and claim.
pub async fn complete_email_claim(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    page_factory: PageFactory,
    flasher: Flasher,
    Form(form): Form<VerifyForm>,
) -> ServerResult<axum::response::Response, StatusCode> {
    // One uniform failure page for unknown/expired/used/wrong-user tokens;
    // the CAS inside `consume` decides, we don't say which check failed.
    let failed = |page_factory: PageFactory| {
        page_factory
            .create_page(
                "Finish Claiming".to_string(),
                Box::new(html! {
                    div class="container" style="max-width: 480px;" {
                        h1 { "That link didn't work" }
                        p {
                            "The claim link is invalid, expired, already used, "
                            "or was requested from a different arena account. "
                            "You can "
                            a href="/claim/email" { "request a fresh one" }
                            "."
                        }
                    }
                }),
            )
            .into_response()
    };

    let Some(imported_account_id) =
        claim_email_token::consume(&state.db, &form.token, user.user_id)
            .await
            .wrap_err("Failed to consume claim token")?
    else {
        return Ok(failed(page_factory));
    };

    let summary = imported_account::claim_account(&state.db, imported_account_id, user.user_id)
        .await
        .wrap_err("Failed to claim imported account")?;

    let Some(summary) = summary else {
        // The account got claimed between link request and click (e.g. by
        // GitHub auto-link in another session). The token is spent either
        // way — a claimed account has nothing left to unlock.
        return Ok(failed(page_factory));
    };

    tracing::info!(
        event_type = "play_claim_succeeded",
        user_id = %user.user_id,
        play_username = %summary.username,
        snakes = summary.snakes_created,
        grants = summary.grants_created,
        "play account claimed via email link"
    );

    // Same security signal as the other claim paths.
    state.mailer.notify_account_claimed(
        &state.db,
        state.config.email_per_recipient_hourly_limit,
        &summary,
    );
    state
        .discord
        .notify_account_claimed(&summary.username, summary.snakes_created);

    flasher
        .add_flash(format!(
            "Welcome back, {}! Brought over {} snake(s) and {} customization unlock(s).",
            summary.username, summary.snakes_created, summary.grants_created
        ))
        .await?;
    Ok(Redirect::to("/battlesnakes").into_response())
}
