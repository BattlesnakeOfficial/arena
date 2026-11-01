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
        achievements::Achievement,
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

fn item_unlocked(
    def: &CustomizationDef,
    kind: &str,
    slug: &str,
    granted: &HashSet<(String, String)>,
) -> bool {
    def.is_free() || granted.contains(&(kind.to_string(), slug.to_string()))
}

fn customization_tabs(achievements_active: bool) -> Markup {
    html! {
        nav class="modes cz-modes" aria-label="Customizations" {
            a class=(if achievements_active { "mode" } else { "mode on" })
                href="/customizations" aria-current=[(!achievements_active).then_some("page")] { "Catalog" }
            a class=(if achievements_active { "mode on" } else { "mode" })
                href="/customizations/achievements" aria-current=[achievements_active.then_some("page")] { "Achievements" }
        }
    }
}

fn guest_sign_in_note() -> Markup {
    html! {
        p class="cz-note" {
            "Browsing as a guest — "
            a href="/auth/github" { "sign in" }
            " to see which cosmetics your account has unlocked."
        }
    }
}

fn root_endpoint_note() -> Markup {
    html! { "snake declares its customizations from its root endpoint" }
}

fn ladder_reward(kind: &str, slug: &str, image_url: &str, def: &CustomizationDef) -> Markup {
    html! {
        div class="cz-step-reward" {
            div class="cz-swatch" { img src=(image_url) alt=(def.display_name) loading="lazy"; }
            div class="cz-step-reward-copy" {
                span class="cz-step-reward-name" { (def.display_name) }
                code { (kind) ": " (slug) }
            }
        }
    }
}

fn achievement_unlocked(achievement: Achievement, granted: &HashSet<(String, String)>) -> bool {
    let def = achievement.def();
    item_unlocked(&def.head.def(), Head::KIND, def.head.slug(), granted)
        && def
            .tail
            .is_none_or(|tail| item_unlocked(&tail.def(), Tail::KIND, tail.slug(), granted))
}

fn ladder_step(
    achievement: Achievement,
    index: usize,
    granted: &HashSet<(String, String)>,
    next_up: bool,
) -> Markup {
    let def = achievement.def();
    let unlocked = achievement_unlocked(achievement, granted);
    html! {
        li class=(if next_up { "cz-step next-up" } else { "cz-step" }) {
            div class="cz-step-marker" {
                span class="sr-only" { "Step " (index + 1) }
                span aria-hidden="true" { @if unlocked { "✓" } @else { (index + 1) } }
            }
            div class="cz-step-card" {
                div class="cz-step-heading" {
                    h3 { (def.name) }
                    span class=(if unlocked { "cz-step-state badge ok" } else { "cz-step-state badge" }) {
                        @if unlocked { "Unlocked" } @else { "Locked" }
                    }
                }
                @if next_up { span class="cz-next-up" { "Next up" } }
                p class="cz-step-description" { (def.description) }
                div class="cz-step-rewards" {
                    (ladder_reward(Head::KIND, def.head.slug(), &def.head.image_url(), &def.head.def()))
                    @if let Some(tail) = def.tail {
                        (ladder_reward(Tail::KIND, tail.slug(), &tail.image_url(), &tail.def()))
                    }
                }
            }
        }
    }
}

fn catalog_item(
    kind: &str,
    slug: &str,
    image_url: &str,
    def: &CustomizationDef,
    granted: &HashSet<(String, String)>,
    signed_in: bool,
    balance: i64,
) -> Markup {
    let unlocked = item_unlocked(def, kind, slug, granted);
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

/// GET /customizations/achievements — browse the Getting Started ladder.
pub async fn achievements_page(
    State(state): State<AppState>,
    OptionalUser(user): OptionalUser,
    page_factory: PageFactory,
) -> ServerResult<impl IntoResponse, StatusCode> {
    let granted = match &user {
        Some(user) => customizations::get_granted_slugs(&state.db, user.user_id)
            .await
            .wrap_err("Failed to fetch customization grants")?,
        None => HashSet::new(),
    };
    let next_up = user.as_ref().and_then(|_| {
        Achievement::GETTING_STARTED
            .iter()
            .position(|achievement| !achievement_unlocked(*achievement, &granted))
    });
    Ok(page_factory.create_page(
        "Achievements".to_string(),
        Box::new(html! {
            div class="page-head" { h1 { "Achievements" } }
            (customization_tabs(true))
            @if user.is_none() { (guest_sign_in_note()) }
            section class="cz-ladder-section" {
                h2 { "Getting Started" }
                ol class="cz-ladder" {
                    @for (index, achievement) in Achievement::GETTING_STARTED.into_iter().enumerate() {
                        (ladder_step(achievement, index, &granted, next_up == Some(index)))
                    }
                }
                div class="cz-ladder-foot" {
                    p { "To wear these rewards, have your snake declare them from its root endpoint." }
                    a class="cz-catalog-link" href="/customizations" { "Browse the full catalog" }
                }
            }
        }),
    ))
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
    let legacy_tokens = match user {
        Some(user) => customizations::legacy_token_total(&state.db, user.user_id).await?,
        None => 0,
    };

    Ok(page_factory.create_page(
        "Customizations".to_string(),
        Box::new(html! {
            div class="page-head" {
                h1 { "Customizations" }
                div class="sub" {
                    "Heads and tails your snakes can wear. A " (root_endpoint_note())
                    "; locked customizations fall back to the default."
                }
            }
            (customization_tabs(false))

            @if user.is_some() {
                @if let Some(message) = redeem_error {
                    p role="alert" { (message) }
                }
                form class="form-stack cz-redeem" method="post" action="/customizations/redeem" {
                    div class="field" {
                        label for="redeem-code" { "Redeem a code" }
                        input id="redeem-code" type="text" name="code" required autocomplete="off" autocapitalize="characters" spellcheck="false";
                    }
                    button type="submit" class="btn solid" { "Redeem" }
                }
                div class="cz-token-panel" aria-label="Token unlocks" {
                    h2 { "Token unlocks" }
                    p class="cz-note" { (balance) " unlock token(s) available" }
                    @if legacy_tokens > 0 {
                        p class="cz-note" { "includes " (legacy_tokens) " token" @if legacy_tokens != 1 { "s" } " from your play points" }
                    }
                    p class="cz-note" { "1 token for every week your snakes play a game; each unlock uses 1 token." }
                    @if balance == 0 {
                        p class="cz-note" { "To earn a token, one of your snakes needs to play a game this week." }
                    }
                }
            }

            @if user.is_none() {
                (guest_sign_in_note())
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
                                @if *group == Group::GettingStarted {
                                    a class="cz-ladder-link" href="/customizations/achievements" { "See the achievement ladder" }
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
    let (status, message) = match outcome {
        RedeemOutcome::Granted { name, kind, slug } => {
            if let Err(error) = flasher
                .success(format!("Unlocked {name} ({kind}: {slug})"))
                .await
            {
                tracing::error!(error = %format!("{error:#}"), "Failed to flash code redemption");
            }
            return Ok(Redirect::to("/customizations").into_response());
        }
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

    fn ladder_steps(page: &str) -> Vec<&str> {
        let ladder = page
            .split("<ol class=\"cz-ladder\">")
            .nth(1)
            .expect("ladder")
            .split("</ol>")
            .next()
            .expect("ladder end");
        ladder
            .split("<li class=\"cz-step")
            .skip(1)
            .map(|step| step.split("</li>").next().expect("step end"))
            .collect()
    }

    fn main_content(page: &str) -> &str {
        page.split_once("<main>")
            .expect("main start")
            .1
            .split_once("</main>")
            .expect("main end")
            .0
    }

    fn assert_tabs(page: &str, active: &str) {
        let nav = page
            .split("<nav class=\"modes cz-modes\" aria-label=\"Customizations\">")
            .nth(1)
            .expect("customization tabs")
            .split("</nav>")
            .next()
            .expect("tabs end");
        assert_eq!(nav.matches("class=\"mode").count(), 2);
        assert_eq!(nav.matches("aria-current=\"page\"").count(), 1);
        assert!(nav.contains("href=\"/customizations\""));
        assert!(nav.contains("href=\"/customizations/achievements\""));
        assert!(nav.contains(active));
        assert!(!nav.contains("/customizations/studio"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn guest_achievement_ladder_and_catalog_tabs(db: sqlx::PgPool) {
        let app = crate::routes::routes(AppState::test_from_pool(db))
            .expect("asset router")
            .into_router();
        let response =
            request_at(&app, Method::GET, "/customizations/achievements", None, "").await;
        assert_eq!(response.status(), StatusCode::OK);
        let page = html(response).await;
        assert!(page.contains("Getting Started"));
        assert!(page.contains("href=\"/auth/github\""));
        assert!(page.contains(
            "To wear these rewards, have your snake declare them from its root endpoint."
        ));
        assert!(page.contains(
            "class=\"cz-catalog-link\" href=\"/customizations\">Browse the full catalog</a>"
        ));
        assert!(!main_content(&page).contains("2024"));
        assert_tabs(
            &page,
            "class=\"mode on\" href=\"/customizations/achievements\" aria-current=\"page\"",
        );
        assert!(page.contains("class=\"active\" href=\"/customizations\""));
        let steps = ladder_steps(&page);
        assert_eq!(steps.len(), 5);
        let mut previous_name_offset = 0;
        for (index, achievement) in Achievement::GETTING_STARTED.into_iter().enumerate() {
            let offset = page
                .find(&format!("<h3>{}</h3>", achievement.def().name))
                .expect("achievement heading");
            assert!(offset > previous_name_offset);
            previous_name_offset = offset;
            assert!(steps[index].contains(achievement.def().name));
            assert!(steps[index].contains(achievement.def().description));
            assert_eq!(steps[index].matches("cz-step-state badge").count(), 1);
            assert!(steps[index].contains("Locked"));
            assert!(!steps[index].contains("Unlocked"));
            assert!(!steps[index].contains("Next up"));
        }
        for slug in [
            "head: frog",
            "head: monkey",
            "tail: monkey",
            "head: subway",
            "tail: subway",
            "head: turtle",
            "tail: turtle",
            "head: judge",
            "tail: judge",
        ] {
            assert_eq!(steps.iter().filter(|step| step.contains(slug)).count(), 1);
        }
        let catalog = html(request_at(&app, Method::GET, "/customizations", None, "").await).await;
        assert_tabs(
            &catalog,
            "class=\"mode on\" href=\"/customizations\" aria-current=\"page\"",
        );
        assert!(!main_content(&catalog).contains("2024"));
        assert!(catalog.contains("See the achievement ladder"));
        let group = catalog
            .split("<section class=\"cz-group\">")
            .find(|section| section.contains("<h2>Getting Started</h2>"))
            .expect("Getting Started group")
            .split("</section>")
            .next()
            .expect("group end");
        assert_eq!(group.matches("class=\"cz-item").count(), 9);
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn achievement_ladder_uses_all_rewards_and_marks_next_step(db: sqlx::PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = crate::routes::routes(state.clone())
            .expect("asset router")
            .into_router();
        let frog_session = create_user_session(&db, 169201, false).await;
        let frog_user = session_user_id(&db, frog_session).await;
        let frog_cookie = signed_session_cookie(&state, frog_session);
        sqlx::query!("INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, $2, $3, 'achievement') ON CONFLICT (user_id, customization_type, slug) DO NOTHING", frog_user, "head", "frog").execute(&db).await.unwrap();
        let frog_page = html(
            request_at(
                &app,
                Method::GET,
                "/customizations/achievements",
                Some(&frog_cookie),
                "",
            )
            .await,
        )
        .await;
        let steps = ladder_steps(&frog_page);
        assert!(steps[0].contains("cz-step-state badge ok\">Unlocked"));
        for step in &steps[1..] {
            assert!(step.contains("cz-step-state badge\">Locked"));
        }
        assert!(steps[1].starts_with(" next-up\""));
        assert!(steps[1].contains("class=\"cz-next-up\">Next up"));
        assert_eq!(
            steps.iter().filter(|step| step.contains("Next up")).count(),
            1
        );

        let turtle_session = create_user_session(&db, 169202, false).await;
        let turtle_user = session_user_id(&db, turtle_session).await;
        let turtle_cookie = signed_session_cookie(&state, turtle_session);
        for (kind, slug) in [("head", "turtle"), ("tail", "turtle")] {
            sqlx::query!("INSERT INTO customization_grants (user_id, customization_type, slug, source) VALUES ($1, $2, $3, 'achievement') ON CONFLICT (user_id, customization_type, slug) DO NOTHING", turtle_user, kind, slug).execute(&db).await.unwrap();
            let page = html(
                request_at(
                    &app,
                    Method::GET,
                    "/customizations/achievements",
                    Some(&turtle_cookie),
                    "",
                )
                .await,
            )
            .await;
            let step = ladder_steps(&page)[3];
            if kind == "head" {
                assert!(step.contains("cz-step-state badge\">Locked"));
            } else {
                assert!(step.contains("cz-step-state badge ok\">Unlocked"));
            }
        }
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn legacy_only_owner_sees_source_note_and_unlock_form(db: sqlx::PgPool) {
        let state = AppState::test_from_pool(db.clone());
        let app = crate::routes::routes(state.clone())
            .expect("asset router")
            .into_router();
        let guest = html(request(&app, Method::GET, None, "").await).await;
        assert!(!guest.contains("tokens from your play points"));
        let session = create_user_session(&db, 164699, false).await;
        let user = session_user_id(&db, session).await;
        let cookie = signed_session_cookie(&state, session);
        let empty = html(request(&app, Method::GET, Some(&cookie), "").await).await;
        assert!(!empty.contains("tokens from your play points"));
        let account = sqlx::query_scalar!(
            "INSERT INTO imported_accounts (play_user_id, play_account_id, email, username, claimed_by_user_id) VALUES ('route-legacy', 'route-legacy-account', 'route@example.com', 'route', $1) RETURNING imported_account_id",
            user,
        ).fetch_one(&db).await.unwrap();
        sqlx::query!("INSERT INTO customization_legacy_tokens (imported_account_id, play_points, token_count) VALUES ($1, 200, 1)", account)
            .execute(&db).await.unwrap();
        let page = html(request(&app, Method::GET, Some(&cookie), "").await).await;
        let panel = page
            .split("class=\"cz-token-panel\"")
            .nth(1)
            .expect("token panel")
            .split("</div>")
            .next()
            .expect("panel end");
        let balance = panel.find("1 unlock token(s) available").expect("balance");
        let note = panel
            .find("includes 1 token from your play points")
            .expect("source note");
        let rule = panel.find("1 token for every week").expect("weekly rule");
        assert!(balance < note && note < rule);
        assert!(page.contains("action=\"/customizations/unlock\""));
        assert!(!page.contains("one of your snakes needs to play a game this week"));
        sqlx::query!(
            "UPDATE customization_legacy_tokens SET play_points = 201, token_count = 2 WHERE imported_account_id = $1",
            account,
        )
        .execute(&db)
        .await
        .unwrap();
        let plural = html(request(&app, Method::GET, Some(&cookie), "").await).await;
        assert!(plural.contains("includes 2 tokens from your play points"));
    }

    #[sqlx::test(migrations = "../migrations")]
    async fn redeem_route_messages_and_hidden_catalog(db: sqlx::PgPool) {
        use crate::models::customization_unlock_code::{CreateCode, create};
        use sha2::Digest as _;
        let state = AppState::test_from_pool(db.clone());
        let app = crate::routes::routes(state.clone())
            .expect("asset router")
            .into_router();
        let guest = html(request_at(&app, Method::GET, "/customizations", None, "").await).await;
        assert!(!guest.contains("action=\"/customizations/redeem\""));
        assert!(!guest.contains("head: hydra"));
        assert!(guest.contains("Getting Started"));
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
        let redeem_form = signed
            .split("<form class=\"form-stack cz-redeem\"")
            .nth(1)
            .expect("redeem form")
            .split("</form>")
            .next()
            .expect("redeem form end");
        assert!(redeem_form.contains("class=\"btn solid\""));
        assert!(redeem_form.contains("id=\"redeem-code\""));
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
        let slug_position = page.rfind("head: hydra").expect("hydra head slug");
        let card_start = page[..slug_position]
            .rfind("<div class=\"cz-item")
            .expect("hydra head card");
        let card_tail = &page[card_start..];
        let card_end = card_tail.find("</section>").expect("end of hydra group");
        let hydra_card = &card_tail[..card_end];
        assert!(hydra_card.contains("badge ok"));
        assert!(hydra_card.contains("Unlocked"));
        assert!(!hydra_card.contains("cz-unlock-form"));
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
            assert!(body.contains("Getting Started"));
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
            .filter(|item| item.def().group == Group::GettingStarted)
            .map(|item| (Head::KIND, item.slug(), item.image_url(), item.def()))
            .chain(
                Tail::ALL
                    .iter()
                    .filter(|item| item.def().group == Group::GettingStarted)
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
        let app = crate::routes::routes(state.clone())
            .expect("asset router")
            .into_router();
        let guest = html(request(&app, Method::GET, None, "").await).await;
        assert!(guest.contains("sign in"));
        assert!(!guest.contains("action=\"/customizations/unlock\""));
        assert!(guest.contains("head: default"));
        assert!(guest.contains("Getting Started"));
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
