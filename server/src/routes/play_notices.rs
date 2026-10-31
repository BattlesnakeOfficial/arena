//! play.battlesnake.com shutdown notices (DEV-1983): the one-click
//! unsubscribe, and an admin page with wave progress, previews, and a
//! send-test button.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use color_eyre::eyre::{Context as _, eyre};
use maud::{Markup, html};

use crate::{
    components::page_factory::PageFactory,
    email::{SendOutcome, messages::EmailMessage},
    errors::{ServerError, ServerResult},
    models::{email_log, notification_email},
    play_shutdown_notices::{self as notices, Wave},
    routes::auth::AdminUser,
    state::AppState,
};

async fn apply_unsubscribe(state: &AppState, token: &str) -> ServerResult<(), StatusCode> {
    let invalid = || {
        ServerError(
            eyre!("Invalid shutdown notice unsubscribe link"),
            StatusCode::BAD_REQUEST,
        )
    };
    let imported_account_id =
        notices::verify_unsubscribe_token(&state.cookie_key, token).ok_or_else(invalid)?;
    if !notices::unsubscribe(&state.db, imported_account_id).await? {
        return Err(invalid());
    }
    Ok(())
}

/// GET /play-notices/unsubscribe/{token}
pub async fn unsubscribe_get(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    apply_unsubscribe(&state, &token).await?;
    Ok(html! {
        html { head { meta name="viewport" content="width=device-width, initial-scale=1"; title { "Unsubscribed" } }
        body style="font: 16px system-ui; margin: 2rem auto; padding: 1rem; max-width: 38rem" {
            h1 { "Unsubscribed" }
            p { "You won't get any more emails about play.battlesnake.com moving." }
            p { "Your account can still come over any time: " a href="/welcome-back" { "bring it to the new site" } "." }
        } }
    })
}

/// POST /play-notices/unsubscribe/{token}: the List-Unsubscribe one-click target.
pub async fn unsubscribe_post(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> ServerResult<StatusCode, StatusCode> {
    apply_unsubscribe(&state, &token).await?;
    Ok(StatusCode::OK)
}

fn eastern(at: DateTime<Utc>) -> String {
    at.with_timezone(&cja::chrono_tz::America::New_York)
        .format("%a %b %-d, %-I:%M %p ET")
        .to_string()
}

fn wave_state(wave: Wave, now: DateTime<Utc>) -> &'static str {
    if now < wave.starts_at() {
        "upcoming"
    } else if now < wave.ends_at() {
        "sending"
    } else {
        "finished"
    }
}

fn sample_message(wave: Wave, to: &str, base_url: &str) -> EmailMessage {
    notices::notice_message(
        wave,
        to,
        "example-player",
        &notices::sample_snakes(),
        base_url,
        &notices::unsubscribe_url(base_url, "sample-invalid-token"),
    )
}

fn parse_wave(number: i16) -> ServerResult<Wave, StatusCode> {
    Wave::from_number(number)
        .ok_or_else(|| ServerError(eyre!("Unknown wave"), StatusCode::NOT_FOUND))
}

async fn admin_content(
    state: &AppState,
    address: Option<&str>,
    outcome: Option<&str>,
) -> ServerResult<Markup, StatusCode> {
    let now = Utc::now();
    let counts = notices::wave_status_counts(&state.db).await?;
    let unsubscribed = notices::unsubscribed_count(&state.db).await?;
    let eligible = match crate::play_import::connect_play(&state.config.database_url, "play").await
    {
        Ok(Some(play)) => {
            let ids = notices::eligible_play_account_ids(&play).await;
            play.close().await;
            match ids {
                Ok(ids) => notices::eligible_recipient_count(&state.db, &ids)
                    .await
                    .map(|count| count.to_string())
                    .unwrap_or_else(|error| format!("unavailable: {error:#}")),
                Err(error) => format!("unavailable: {error:#}"),
            }
        }
        Ok(None) => "unavailable: no play database".to_string(),
        Err(error) => format!("unavailable: {error:#}"),
    };
    let count = |wave: Wave, status: &str| {
        counts
            .iter()
            .find(|(w, s, _)| *w == wave.number() && s == status)
            .map_or(0, |(_, _, n)| *n)
    };

    Ok(html! {
        div style="min-width:0;max-width:100%;font-size:16px;overflow-wrap:anywhere;" {
            nav { a href="/admin" { "← Admin" } }
            h1 { "Play shutdown notices" }
            @if let Some(outcome) = outcome {
                p role="status" { (outcome) }
            }
            p {
                @if state.config.play_shutdown_notices_enabled {
                    strong { "Armed." } " Sending runs on the schedule below."
                } @else {
                    strong { "Not armed." } " Nothing sends until PLAY_SHUTDOWN_NOTICES_ENABLED=true."
                }
            }
            p { "Would reach right now: " (eligible) " · Unsubscribed: " (unsubscribed) }
            p { "play.battlesnake.com redirects " (eastern(notices::redirect_at())) "." }
            table style="border-collapse:collapse;margin:12px 0 20px;" {
                thead { tr { @for heading in ["Wave", "Sends", "State", "Sent", "Failed", "In flight"] { th style="text-align:left;padding:6px 16px 6px 0;" { (heading) } } } }
                tbody {
                    @for wave in Wave::ALL {
                        tr {
                            @for cell in [
                                wave.number().to_string(),
                                format!("{} – {}", eastern(wave.starts_at()), eastern(wave.ends_at())),
                                wave_state(wave, now).to_string(),
                                count(wave, "sent").to_string(),
                                count(wave, "failed").to_string(),
                                count(wave, "sending").to_string(),
                            ] {
                                td style="padding:6px 16px 6px 0;" { (cell) }
                            }
                        }
                    }
                }
            }
            h2 { "Send test" }
            @if let Some(address) = address {
                p { "Tests go to your resolved notification address: " (address) }
            } @else {
                p { "No notification address is available for your account. Add one before sending a test." }
            }
            @for wave in Wave::ALL {
                @let message = sample_message(wave, "sample@example.com", &state.config.base_url);
                details {
                    summary { "Wave " (wave.number()) ": " (message.subject) }
                    form method="post" action=(format!("/admin/play-notices/{}/send-test", wave.number())) {
                        button type="submit" disabled[address.is_none()] style="min-height:44px;padding:10px 18px;font-size:16px;" { "Send wave " (wave.number()) " test to me" }
                    }
                    iframe title=(format!("Wave {} HTML preview", wave.number())) sandbox="" srcdoc=(message.html) style="display:block;width:100%;max-width:100%;height:600px;border:1px solid var(--hairline);box-sizing:border-box;" {}
                    pre style="white-space:pre-wrap;max-width:100%;" { (message.text) }
                }
            }
        }
    })
}

/// GET /admin/play-notices
pub async fn admin_page(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let mut connection = state.db.acquire().await?;
    let address = notification_email::resolved_recipient(&mut connection, user.user_id).await?;
    drop(connection);
    let content = admin_content(&state, address.as_deref(), None).await?;
    Ok(page_factory.create_page("Play shutdown notices".into(), Box::new(content)))
}

/// POST /admin/play-notices/{wave}/send-test: a sample of that wave to the
/// admin's own resolved address.
pub async fn send_test(
    State(state): State<AppState>,
    AdminUser(user): AdminUser,
    Path(wave): Path<i16>,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let wave = parse_wave(wave)?;
    let mut tx = state.db.begin().await?;
    let address = notification_email::resolved_recipient(&mut tx, user.user_id).await?;
    let mut outcome = "No notification address is available; no test was sent.".to_string();
    if let Some(to) = &address {
        let message = sample_message(wave, to, &state.config.base_url);
        outcome = match state
            .mailer
            .send_limited(
                &mut tx,
                state.config.email_per_recipient_hourly_limit,
                email_log::ADMIN_TEST_PURPOSE,
                &message,
            )
            .await
        {
            Ok(SendOutcome::Sent) => format!("Sent wave {} test to {to}", wave.number()),
            Ok(SendOutcome::Disabled) => "Disabled (no Mailgun config)".into(),
            Ok(SendOutcome::RateLimited) => "RateLimited".into(),
            Err(err) => {
                tracing::error!(error = %format!("{err:#}"), "Shutdown notice test email failed");
                format!("{err:#}")
            }
        };
    }
    tx.commit()
        .await
        .wrap_err("Failed to commit shutdown notice test attempt")?;
    let content = admin_content(&state, address.as_deref(), Some(&outcome)).await?;
    Ok(page_factory.create_page("Play shutdown notices".into(), Box::new(content)))
}
