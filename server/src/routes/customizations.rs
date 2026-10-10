use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Redirect},
};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use serde::Deserialize;
use std::collections::HashSet;

use crate::{
    components::{page::Page, page_factory::PageFactory},
    customizations::{
        self, Availability, CustomizationDef, Group, Head, Tail, UnlockOutcome, achievements,
    },
    errors::{ServerResult, WithStatus},
    flasher::Flasher,
    models::{
        customization_unlock_code::{self as codes, RedeemOutcome},
        user::User,
    },
    routes::auth::{CurrentUser, OptionalUser},
    state::AppState,
};

fn catalog_item(
    kind: &str,
    slug: &str,
    image_url: &str,
    def: &CustomizationDef,
    granted: &HashSet<(String, String)>,
    signed_in: bool,
    balance: i64,
) -> Markup {
    let unlocked = def.is_free() || granted.contains(&(kind.to_string(), slug.to_string()));
    let achievement = achievements::for_item(kind, slug);

    html! {
        div .cz-item .locked[!unlocked] {
            div class="cz-swatch" {
                img src=(image_url) alt="" loading="lazy";
            }
            div class="cz-name" title=(def.display_name) { (def.display_name) }
            code class="cz-slug" { (kind) ": " (slug) }
            @if let Some(achievement) = achievement {
                p class="cz-description" { (achievement.name) ": " (achievement.description) }
            } @else if !def.description.is_empty() {
                p class="cz-description" { (def.description) }
            }
            @if unlocked {
                span class="badge ok" { "Unlocked" }
            } @else {
                span class="badge" { "Locked" }
            }
            @if signed_in && !unlocked && def.is_token_unlockable() && balance > 0 {
                form class="cz-unlock-form" method="post" action="/customizations/unlock" {
                    input type="hidden" name="kind" value=(kind);
                    input type="hidden" name="slug" value=(slug);
                    button type="submit" class="btn solid cz-unlock" { "Unlock" }
                }
            }
        }
    }
}

/// GET /customizations — browse the head/tail cosmetic catalog
pub async fn list_customizations(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    Ok(render_customizations_page(&state, user.as_ref(), page_factory, None).await?)
}

async fn render_customizations_page(
    state: &AppState,
    user: Option<&User>,
    page_factory: PageFactory,
    redeem_error: Option<&str>,
) -> cja::Result<Page> {
    let granted = match user {
        Some(user) => customizations::get_granted_slugs(&state.db, user.user_id)
            .await
            .wrap_err("Failed to fetch customization grants")?,
        None => HashSet::new(),
    };
    let balance = match user {
        Some(user) => customizations::token_balance(&state.db, user.user_id).await?,
        None => 0,
    };

    Ok(page_factory.create_page(
        "Customizations".to_string(),
        Box::new(html! {
            div class="page-head" {
                h1 { "Customizations" }
                div class="sub" {
                    "Heads and tails your snakes can wear. A snake declares its "
                    "customizations from its root endpoint; locked customizations "
                    "fall back to the default."
                }
            }

            @if user.is_some() {
                @if let Some(message) = redeem_error {
                    p role="alert" { (message) }
                }
                form method="post" action="/customizations/redeem" {
                    label for="redeem-code" { "Redeem a code" }
                    input id="redeem-code" type="text" name="code" required;
                    button type="submit" { "Redeem" }
                }
                div class="cz-token-panel" aria-label="Token unlocks" {
                    h2 { "Token unlocks" }
                    p class="cz-note" { (balance) " unlock token(s) available" }
                    p class="cz-note" { "1 token for every week your snakes play a game; each unlock uses 1 token." }
                    @if balance == 0 {
                        p class="cz-note" { "To earn a token, one of your snakes needs to play a game this week." }
                    }
                }
            }

            @if user.is_none() {
                p class="cz-note" {
                    "Browsing as a guest — "
                    a href="/auth/github" { "sign in" }
                    " to see which cosmetics your account has unlocked."
                }
            }

            @for group in Group::ALL {
                {
                    @let heads: Vec<_> = Head::ALL.iter().filter(|h| h.def().group == *group && (group.availability() != Availability::Hidden || granted.contains(&(Head::KIND.to_string(), h.slug().to_string())))).collect();
                    @let tails: Vec<_> = Tail::ALL.iter().filter(|t| t.def().group == *group && (group.availability() != Availability::Hidden || granted.contains(&(Tail::KIND.to_string(), t.slug().to_string())))).collect();
                    @if !heads.is_empty() || !tails.is_empty() {
                        section class="cz-group" {
                            div class="cz-group-head" {
                                h2 { (group.title()) }
                                span class="cz-count" {
                                    (heads.len() + tails.len()) " items"
                                }
                            }
                            @if !heads.is_empty() {
                                h3 class="cz-kind" { "Heads" }
                                div class="cz-grid" {
                                    @for head in &heads {
                                        (catalog_item(Head::KIND, head.slug(), &head.image_url(), &head.def(), &granted, user.is_some(), balance))
                                    }
                                }
                            }
                            @if !tails.is_empty() {
                                h3 class="cz-kind" { "Tails" }
                                div class="cz-grid" {
                                    @for tail in &tails {
                                        (catalog_item(Tail::KIND, tail.slug(), &tail.image_url(), &tail.def(), &granted, user.is_some(), balance))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }),
    ))
}

#[derive(Deserialize)]
pub struct RedeemCodeForm {
    code: String,
}

pub async fn redeem_code(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    page_factory: PageFactory,
    flasher: Flasher,
    Form(form): Form<RedeemCodeForm>,
) -> ServerResult<axum::response::Response, StatusCode> {
    let outcome = codes::redeem(&state.db, user.user_id, &form.code).await?;
    if let RedeemOutcome::Granted { name, kind, slug } = outcome {
        if let Err(error) = flasher
            .success(format!("Unlocked {name} ({kind}: {slug})"))
            .await
        {
            tracing::error!(error = %format!("{error:#}"), "Failed to flash code redemption");
        }
        return Ok(Redirect::to("/customizations").into_response());
    }
    let (status, message) = match outcome {
        RedeemOutcome::Unknown => (StatusCode::NOT_FOUND, "Unknown code"),
        RedeemOutcome::Expired => (StatusCode::GONE, "This code has expired"),
        RedeemOutcome::Disabled => (StatusCode::FORBIDDEN, "This code is disabled"),
        RedeemOutcome::FullyRedeemed => (StatusCode::CONFLICT, "This code has been fully redeemed"),
        RedeemOutcome::AlreadyRedeemed => (StatusCode::CONFLICT, "You already redeemed this code"),
        RedeemOutcome::AlreadyOwned => (
            StatusCode::CONFLICT,
            "You already unlocked this item. The code wasn't used.",
        ),
        RedeemOutcome::RateLimited => (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed redemptions. Try again later",
        ),
        RedeemOutcome::Granted { .. } => unreachable!(),
    };
    Ok((
        status,
        render_customizations_page(&state, Some(&user), page_factory, Some(message)).await?,
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct UnlockForm {
    kind: String,
    slug: String,
}

pub async fn unlock_customization(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    flasher: Flasher,
    Form(form): Form<UnlockForm>,
) -> ServerResult<impl IntoResponse, StatusCode> {
    match customizations::unlock_with_token(&state.db, user.user_id, &form.kind, &form.slug).await?
    {
        UnlockOutcome::Unlocked(name) => {
            if let Err(error) = flasher.add_flash(format!("Unlocked {name}")).await {
                tracing::error!(error = %format!("{error:#}"), "Failed to flash customization unlock");
            }
            Ok(Redirect::to("/customizations"))
        }
        UnlockOutcome::UnknownItem => {
            Err("Unknown customization".to_string()).with_status(StatusCode::NOT_FOUND)
        }
        UnlockOutcome::NotTokenUnlockable => {
            Err("Customization cannot be unlocked with a token".to_string())
                .with_status(StatusCode::UNPROCESSABLE_ENTITY)
        }
        UnlockOutcome::AlreadyOwned => {
            Err("Customization already unlocked".to_string()).with_status(StatusCode::CONFLICT)
        }
        UnlockOutcome::InsufficientTokens => {
            Err("No unlock tokens available".to_string()).with_status(StatusCode::CONFLICT)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::test_support::{
        create_user_session, session_user_id, signed_session_cookie,
    };
    use axum::{
        body::{Body, to_bytes},
        http::{Method, Request, header},
    };
    use tower::ServiceExt as _;

    async fn request(
        app: &axum::Router,
        method: Method,
        cookie: Option<&str>,
        body: &str,
    ) -> axum::response::Response {
        let uri = if body.is_empty() {
            "/customizations"
        } else {
            "/customizations/unlock"
        };
        request_at(app, method, uri, cookie, body).await
    }

    async fn request_at(
        app: &axum::Router,
        method: Method,
        uri: &str,
        cookie: Option<&str>,
        body: &str,
    ) -> axum::response::Response {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(cookie) = cookie {
            builder = builder.header(
                header::COOKIE,
                format!("{}={cookie}", crate::models::session::SESSION_COOKIE_NAME),
            );
        }
        if !body.is_empty() {
            builder = builder.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        }
        app.clone()
            .oneshot(builder.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap()
    }

    async fn html(response: axum::response::Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn redeem_route_messages_and_hidden_catalog(db: sqlx::PgPool) {
        use crate::models::customization_unlock_code::{CreateCode, create};
        use sha2::Digest as _;
        let state = AppState::test_from_pool(db.clone());
        let app =
            crate::routes::routes(state.clone()).layer(tower_cookies::CookieManagerLayer::new());
        let guest = html(request_at(&app, Method::GET, "/customizations", None, "").await).await;
        assert!(!guest.contains("action=\"/customizations/redeem\""));
        assert!(!guest.contains("head: hydra"));
        assert!(guest.contains("2024 Achievement Collection"));
        assert_eq!(
            request_at(
                &app,
                Method::POST,
                "/customizations/redeem",
                None,
                "code=bad"
            )
            .await
            .status(),
            StatusCode::UNAUTHORIZED
        );
        let session = create_user_session(&db, 164561, false).await;
        let player = session_user_id(&db, session).await;
        let cookie = signed_session_cookie(&state, session);
        let signed =
            html(request_at(&app, Method::GET, "/customizations", Some(&cookie), "").await).await;
        assert!(signed.contains("action=\"/customizations/redeem\""));
        assert!(!signed.contains("head: hydra"));
        let input = CreateCode {
            customization_type: "head".into(),
            slug: "hydra".into(),
            max_redemptions: 2,
            expires_at: None,
            note: None,
        };
        let code = create(&db, player, &input).await.unwrap();
        let before = customizations::token_balance(&db, player).await.unwrap();
        let response = request_at(
            &app,
            Method::POST,
            "/customizations/redeem",
            Some(&cookie),
            &format!("code={code}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers()[header::LOCATION], "/customizations");
        let page =
            html(request_at(&app, Method::GET, "/customizations", Some(&cookie), "").await).await;
        assert!(page.contains("Unlocked Community Hydra (head: hydra)"));
        assert!(page.contains("head: hydra"));
        assert!(!page.contains("tail: hydra"));
        assert!(page.contains("Unlocked"));
        assert_eq!(
            customizations::token_balance(&db, player).await.unwrap(),
            before
        );
        let expired = create(&db, player, &input).await.unwrap();
        sqlx::query("UPDATE customization_unlock_codes SET expires_at = NOW() - INTERVAL '1 second' WHERE code_hash = $1")
            .bind(hex::encode(sha2::Sha256::digest(expired.as_bytes()))).execute(&db).await.unwrap();
        let disabled = create(&db, player, &input).await.unwrap();
        let disabled_id: uuid::Uuid = sqlx::query_scalar(
            "SELECT code_id FROM customization_unlock_codes WHERE code_hash = $1",
        )
        .bind(hex::encode(sha2::Sha256::digest(disabled.as_bytes())))
        .fetch_one(&db)
        .await
        .unwrap();
        codes::disable(&db, disabled_id).await.unwrap();
        let owned = create(&db, player, &input).await.unwrap();
        let full = create(
            &db,
            player,
            &CreateCode {
                customization_type: "head".into(),
                slug: "turtle".into(),
                max_redemptions: 1,
                expires_at: None,
                note: None,
            },
        )
        .await
        .unwrap();
        let other_session = create_user_session(&db, 164562, false).await;
        let other_cookie = signed_session_cookie(&state, other_session);
        assert_eq!(
            request_at(
                &app,
                Method::POST,
                "/customizations/redeem",
                Some(&other_cookie),
                &format!("code={full}")
            )
            .await
            .status(),
            StatusCode::SEE_OTHER
        );
        for (code, status, message) in [
            ("bad".to_string(), StatusCode::NOT_FOUND, "Unknown code"),
            (
                code.clone(),
                StatusCode::CONFLICT,
                "You already redeemed this code",
            ),
            (expired, StatusCode::GONE, "This code has expired"),
            (disabled, StatusCode::FORBIDDEN, "This code is disabled"),
            (
                owned,
                StatusCode::CONFLICT,
                "You already unlocked this item. The code wasn't used.",
            ),
            (
                full,
                StatusCode::CONFLICT,
                "This code has been fully redeemed",
            ),
        ] {
            let response = request_at(
                &app,
                Method::POST,
                "/customizations/redeem",
                Some(&cookie),
                &format!("code={code}"),
            )
            .await;
            assert_eq!(response.status(), status);
            let body = html(response).await;
            assert!(body.contains(message));
            assert!(body.contains("2024 Achievement Collection"));
            assert!(body.contains("Token unlocks"));
            assert!(body.contains(achievements::Achievement::ALL[0].def().description));
            for word in message
                .to_ascii_lowercase()
                .split(|c: char| !c.is_ascii_alphabetic())
            {
                assert!(
                    !["buy", "price", "paid", "owned", "free", "cost", "costs"].contains(&word)
                );
            }
        }
        let limited_session = create_user_session(&db, 164563, false).await;
        let limited_cookie = signed_session_cookie(&state, limited_session);
        for _ in 0..10 {
            assert_eq!(
                request_at(
                    &app,
                    Method::POST,
                    "/customizations/redeem",
                    Some(&limited_cookie),
                    "code=bad"
                )
                .await
                .status(),
                StatusCode::NOT_FOUND
            );
        }
        let limited = request_at(
            &app,
            Method::POST,
            "/customizations/redeem",
            Some(&limited_cookie),
            "code=bad",
        )
        .await;
        assert_eq!(limited.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            html(limited)
                .await
                .contains("Too many failed redemptions. Try again later")
        );
    }

    #[test]
    fn achievement_cards_explain_rewards_without_token_actions() {
        let mut owned = HashSet::new();
        for (kind, slug, url, def) in Head::ALL
            .iter()
            .filter(|item| item.def().group == Group::Collection2024)
            .map(|item| (Head::KIND, item.slug(), item.image_url(), item.def()))
            .chain(
                Tail::ALL
                    .iter()
                    .filter(|item| item.def().group == Group::Collection2024)
                    .map(|item| (Tail::KIND, item.slug(), item.image_url(), item.def())),
            )
        {
            let achievement = achievements::for_item(kind, slug).unwrap();
            let guest = catalog_item(kind, slug, &url, &def, &owned, false, 0).into_string();
            let locked = catalog_item(kind, slug, &url, &def, &owned, true, 4).into_string();
            for card in [&guest, &locked] {
                assert!(card.contains(achievement.name));
                assert!(card.contains(achievement.description));
                assert!(card.contains("Locked"));
                assert!(!card.contains("cz-unlock-form"));
                for word in card
                    .to_ascii_lowercase()
                    .split(|c: char| !c.is_ascii_alphabetic())
                {
                    assert!(
                        !["buy", "price", "paid", "owned", "free", "cost", "costs"].contains(&word)
                    );
                }
            }
            owned.insert((kind.to_string(), slug.to_string()));
            let unlocked = catalog_item(kind, slug, &url, &def, &owned, true, 4).into_string();
            assert!(unlocked.contains("Unlocked"));
            assert!(!unlocked.contains("cz-unlock-form"));
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn guest_zero_balance_and_unlock_form(db: sqlx::PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app =
            crate::routes::routes(state.clone()).layer(tower_cookies::CookieManagerLayer::new());
        let guest = html(request(&app, Method::GET, None, "").await).await;
        assert!(guest.contains("sign in"));
        assert!(!guest.contains("action=\"/customizations/unlock\""));
        assert!(guest.contains("head: default"));
        assert!(guest.contains("2024 Achievement Collection"));
        for achievement in achievements::Achievement::ALL {
            assert!(guest.contains(achievement.def().description));
        }
        assert_eq!(
            request(&app, Method::POST, None, "kind=head&slug=alligator")
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );

        let session = create_user_session(&db, 164701, false).await;
        let user = session_user_id(&db, session).await;
        let cookie = signed_session_cookie(&state, session);
        let empty = html(request(&app, Method::GET, Some(&cookie), "").await).await;
        assert!(
            empty.contains(
                "1 token for every week your snakes play a game; each unlock uses 1 token."
            )
        );
        assert!(empty.contains("one of your snakes needs to play a game this week"));
        assert!(!empty.contains("action=\"/customizations/unlock\""));

        sqlx::query!("INSERT INTO customization_active_weeks (user_id, week_start) VALUES ($1, '2026-10-05')", user).execute(&db).await.unwrap();
        let funded = html(request(&app, Method::GET, Some(&cookie), "").await).await;
        assert!(funded.contains("1 unlock token(s) available"));
        assert!(funded.contains("head: alligator"));
        assert!(funded.contains("tail: alligator"));
        assert!(funded.contains("action=\"/customizations/unlock\""));
        let intro = funded
            .split("<h1>Customizations</h1>")
            .nth(1)
            .expect("customization heading")
            .split("<section class=\"cz-group\">")
            .next()
            .expect("intro before catalog");
        for word in intro
            .to_ascii_lowercase()
            .split(|c: char| !c.is_ascii_alphabetic())
        {
            assert!(
                !["buy", "price", "paid", "owned", "free", "cost", "costs"].contains(&word),
                "purchase vocabulary in customization page intro: {word}"
            );
        }

        for (body, expected) in [
            ("kind=head&slug=not-real", StatusCode::NOT_FOUND),
            ("kind=unknown&slug=alligator", StatusCode::NOT_FOUND),
            ("kind=head&slug=default", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=head&slug=fish", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=head&slug=turtle", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=tail&slug=turtle", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=head&slug=frog", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=head&slug=judge", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=tail&slug=judge", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=head&slug=monkey", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=tail&slug=monkey", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=head&slug=subway", StatusCode::UNPROCESSABLE_ENTITY),
            ("kind=tail&slug=subway", StatusCode::UNPROCESSABLE_ENTITY),
        ] {
            assert_eq!(
                request(&app, Method::POST, Some(&cookie), body)
                    .await
                    .status(),
                expected
            );
            assert_eq!(customizations::token_balance(&db, user).await.unwrap(), 1);
        }
        let unlocked = request(
            &app,
            Method::POST,
            Some(&cookie),
            "kind=head&slug=alligator",
        )
        .await;
        assert_eq!(unlocked.status(), StatusCode::SEE_OTHER);
        assert_eq!(unlocked.headers()[header::LOCATION], "/customizations");
        assert_eq!(customizations::token_balance(&db, user).await.unwrap(), 0);
        assert_eq!(
            request(
                &app,
                Method::POST,
                Some(&cookie),
                "kind=tail&slug=alligator"
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        let page = html(request(&app, Method::GET, Some(&cookie), "").await).await;
        assert!(page.contains(&format!("Unlocked {}", Head::Alligator.def().display_name)));
        assert!(!page.contains("action=\"/customizations/unlock\""));
        assert_eq!(
            request(
                &app,
                Method::POST,
                Some(&cookie),
                "kind=head&slug=alligator"
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
    }
}
