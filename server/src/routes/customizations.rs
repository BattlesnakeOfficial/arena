use axum::{extract::State, http::StatusCode, response::IntoResponse};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use std::collections::HashSet;

use crate::{
    components::page_factory::PageFactory,
    customizations::{self, Availability, CustomizationDef, Group, Head, Tail},
    errors::ServerResult,
    routes::auth::OptionalUser,
    state::AppState,
};

fn catalog_item(
    kind: &str,
    slug: &str,
    image_url: &str,
    def: &CustomizationDef,
    granted: &HashSet<(String, String)>,
) -> Markup {
    let owned = granted.contains(&(kind.to_string(), slug.to_string()));
    let free = def.is_free();
    let locked = !owned && !free;

    html! {
        div .cz-item .locked[locked] {
            div class="cz-swatch" {
                img src=(image_url) alt="" loading="lazy";
            }
            div class="cz-name" title=(def.display_name) { (def.display_name) }
            @if owned {
                span class="badge ok" { "Owned" }
            } @else if free {
                span class="badge" { "Free" }
            } @else {
                span class="badge" { "Locked" }
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
    let granted = match &user {
        Some(user) => customizations::get_granted_slugs(&state.db, user.user_id)
            .await
            .wrap_err("Failed to fetch customization grants")?,
        None => HashSet::new(),
    };

    Ok(page_factory.create_page(
        "Customizations".to_string(),
        Box::new(html! {
            div class="page-head" {
                h1 { "Customizations" }
                div class="sub" {
                    "Heads and tails your snakes can wear. A snake declares its "
                    "customizations from its root endpoint; anything you don't have "
                    "access to falls back to the default."
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
                @if group.availability() != Availability::Hidden {
                    @let heads: Vec<_> = Head::ALL.iter().filter(|h| h.def().group == *group).collect();
                    @let tails: Vec<_> = Tail::ALL.iter().filter(|t| t.def().group == *group).collect();
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
                                        (catalog_item(Head::KIND, head.slug(), &head.image_url(), &head.def(), &granted))
                                    }
                                }
                            }
                            @if !tails.is_empty() {
                                h3 class="cz-kind" { "Tails" }
                                div class="cz-grid" {
                                    @for tail in &tails {
                                        (catalog_item(Tail::KIND, tail.slug(), &tail.image_url(), &tail.def(), &granted))
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
