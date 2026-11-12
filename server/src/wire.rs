//! Wire types re-exported from `engine-core` (see `engine_core::wire`).
//! Conversion to the Battlesnake API JSON happens in the shared crate so
//! the battlesnake CLI sends byte-identical requests.

pub use engine_core::wire::*;
