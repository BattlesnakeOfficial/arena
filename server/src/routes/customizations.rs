use axum::{extract::State, http::StatusCode, response::IntoResponse};
use color_eyre::eyre::Context as _;
use maud::{Markup, html};
use std::collections::HashSet;
use uuid::Uuid;

use crate::{
    components::page_factory::PageFactory,
    errors::ServerResult,
    models::customization::{self, CatalogEntry},
    routes::auth::OptionalUser,
    state::AppState,
};

fn catalog_item(entry: &CatalogEntry, granted: &HashSet<Uuid>) -> Markup {
    let owned = granted.contains(&entry.customization_id);
    let free = entry.is_free();
    let locked = !owned && !free;

    html! {
        div style={
            "border: 1px solid #ddd; border-radius: 8px; padding: 12px; width: 140px; text-align: center;"
            @if locked { " opacity: 0.5;" }
        } {
            img src=(entry.image_url) alt=(entry.display_name)
                style="width: 64px; height: 64px;" loading="lazy";
            div style="font-weight: bold; margin-top: 8px;" { (entry.display_name) }
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
    let catalog = customization::get_visible_catalog(&state.db)
        .await
        .wrap_err("Failed to fetch customization catalog")?;

    let granted = match &user {
        Some(user) => customization::get_granted_customization_ids(&state.db, user.user_id)
            .await
            .wrap_err("Failed to fetch customization grants")?,
        None => HashSet::new(),
    };

    // Entries arrive ordered by group ordinal; partition into groups while
    // preserving that order.
    let mut groups: Vec<(&str, Vec<&CatalogEntry>)> = Vec::new();
    for entry in &catalog {
        match groups.last_mut() {
            Some((title, entries)) if *title == entry.group_title => entries.push(entry),
            _ => groups.push((&entry.group_title, vec![entry])),
        }
    }

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

                @for (group_title, entries) in &groups {
                    h2 style="margin-top: 24px;" { (group_title) }
                    @let heads: Vec<_> = entries.iter().filter(|e| e.customization_type == "head").collect();
                    @let tails: Vec<_> = entries.iter().filter(|e| e.customization_type == "tail").collect();
                    @if !heads.is_empty() {
                        h3 { "Heads" }
                        div style="display: flex; flex-wrap: wrap; gap: 12px;" {
                            @for entry in &heads { (catalog_item(entry, &granted)) }
                        }
                    }
                    @if !tails.is_empty() {
                        h3 { "Tails" }
                        div style="display: flex; flex-wrap: wrap; gap: 12px;" {
                            @for entry in &tails { (catalog_item(entry, &granted)) }
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
