use axum::{extract::FromRequestParts, http::request::Parts, response::Response};
use maud::Render;

use crate::{
    components::{flash::Flash, page::Page},
    models::user::User,
    routes::auth::OptionalUser,
    state::AppState,
};

/// PageFactory extractor
///
/// This extractor is responsible for creating Page instances with all
/// necessary shell context: flash messages, the logged-in user (for the
/// nav and theme preferences), and the request path (for nav highlighting).
pub struct PageFactory {
    /// The flash message extracted from the session (already cleared from DB)
    pub flash: Flash,
    /// The logged-in user, if any
    pub user: Option<User>,
    /// The request path, used for nav active states
    pub path: String,
}

impl PageFactory {
    /// Create a new Page with the extracted flash message (if any)
    pub fn create_page(self, title: String, content: Box<dyn Render>) -> Page {
        self.build(title, content, false)
    }

    /// Create a new Page with an explicit flash message
    /// This is useful when you want to use the FlashData extractor but also
    /// add it to the page later
    pub fn create_page_with_flash(
        self,
        title: String,
        content: Box<dyn Render>,
        flash: Flash,
    ) -> Page {
        let mut page = self.build(title, content, false);
        page.flash = flash.message;
        page.flash_type = flash.flash_type;
        page
    }

    /// Create a Page that resolves its theme from the game-theater axis
    /// instead of the site axis (game live/replay pages).
    pub fn create_theater_page(self, title: String, content: Box<dyn Render>) -> Page {
        self.build(title, content, true)
    }

    fn build(self, title: String, content: Box<dyn Render>, theater: bool) -> Page {
        Page {
            title,
            content,
            flash: self.flash.message,
            flash_type: self.flash.flash_type,
            user: self.user,
            current_path: self.path,
            theater,
        }
    }
}

impl FromRequestParts<AppState> for PageFactory {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let path = parts.uri.path().to_string();
        let flash = Flash::from_request_parts(parts, state).await?;
        let OptionalUser(user) = OptionalUser::from_request_parts(parts, state).await?;
        Ok(Self { flash, user, path })
    }
}
