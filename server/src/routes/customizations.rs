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
        div style={
            "border: 1px solid #ddd; border-radius: 8px; padding: 12px; width: 140px; text-align: center;"
            @if locked { " opacity: 0.5;" }
        } {
            img src=(image_url) alt=(def.display_name)
                style="width: 64px; height: 64px;" loading="lazy";
            div style="font-weight: bold; margin-top: 8px;" { (def.display_name) }
            @if owned {
                span class="badge bg-success text-white" { "Owned" }
            } @else if free {
                span class="badge bg-secondary text-white" { "Free" }
            } @else {
                span class="badge bg-secondary text-white" { "Locked" }
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
            div class="container" {
                h1 { "Customizations" }
                p {
                    "Heads and tails your snakes can wear. Your snake declares its "
                    "customizations in the response from its root endpoint; anything "
                    "you don't have access to falls back to the default."
                }

                @for group in Group::ALL {
                    @if group.availability() != Availability::Hidden {
                        @let heads: Vec<_> = Head::ALL.iter().filter(|h| h.def().group == *group).collect();
                        @let tails: Vec<_> = Tail::ALL.iter().filter(|t| t.def().group == *group).collect();
                        @if !heads.is_empty() || !tails.is_empty() {
                            h2 style="margin-top: 24px;" { (group.title()) }
                            @if !heads.is_empty() {
                                h3 { "Heads" }
                                div style="display: flex; flex-wrap: wrap; gap: 12px;" {
                                    @for head in &heads {
                                        (catalog_item(Head::KIND, head.slug(), &head.image_url(), &head.def(), &granted))
                                    }
                                }
                            }
                            @if !tails.is_empty() {
                                h3 { "Tails" }
                                div style="display: flex; flex-wrap: wrap; gap: 12px;" {
                                    @for tail in &tails {
                                        (catalog_item(Tail::KIND, tail.slug(), &tail.image_url(), &tail.def(), &granted))
                                    }
                                }
                            }
                        }
                    }
                }

                div class="nav" style="margin-top: 20px;" {
                    a href="/" { "Back to Home" }
                }
            }
        }),
    ))
}
