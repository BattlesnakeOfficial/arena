//! Head & Tail Studio (DEV-1539): a public, login-free page where an artist uploads a
//! drawing of a Battlesnake head or tail and previews it on a real board.
//!
//! - [`page`]: `GET /customizations/studio`, the page (server-rendered boards; the
//!   client is `static/studio.js`).
//! - [`guide`]: `GET /customizations/studio/guide`, how to draw a head or tail, with the
//!   templates to download (`static/design-kit/`, from `scripts/design-kit/`).
//! - [`process`]: `POST /customizations/studio/process`, the guarded processing
//!   endpoint, which runs each upload in an `arena studio-worker` child process.
//!
//! Nothing is stored on the server. Not linked from the rest of the site yet (DEV-1539:
//! no footer or `/customizations` link until launch); `/studio` redirects here.

pub mod guide;
pub mod page;
pub mod process;

pub use guide::guide_page;
pub use page::studio_page;
pub use process::{StudioState, process_route};

#[cfg(test)]
mod tests;
