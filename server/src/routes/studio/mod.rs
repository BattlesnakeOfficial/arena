//! Head & Tail Studio (DEV-1539): a public, login-free page where an artist uploads a
//! drawing of a Battlesnake head or tail and previews it on a real board.
//!
//! - [`page`]: `GET /customizations/studio`, the page (server-rendered boards; the
//!   client is `static/studio.js`).
//! - [`process`]: `POST /customizations/studio/process`, the guarded processing
//!   endpoint, which runs each upload in an `arena studio-worker` child process.
//!
//! Nothing is stored on the server. Not linked from the nav, footer or customizations
//! page yet (DEV-1539 PR 4).

pub mod page;
pub mod process;

pub use page::studio_page;
pub use process::{StudioState, process_route};

#[cfg(test)]
mod tests;
