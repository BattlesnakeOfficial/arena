//! Final standings of a finished game.
//!
//! Snakes are ranked by how long they lasted: survivors first, then by the
//! turn they were eliminated, latest first. Snakes that lasted equally long
//! share a placement (competition ranking: 1, 2, 2, 4), the way Play ranked
//! games. So two snakes that die in the same head-to-head tie rather than one
//! of them winning on roster order.
//!
//! A game's winner is the one snake placed first on its own. A shared first
//! place is a draw: nobody won.
//!
//! The implementation lives in engine-core (shared with the battlesnake
//! CLI's `play`); re-exported so every existing `crate::placement::…` call
//! site keeps compiling.

pub use engine_core::placement::{from_final_snakes, outright_winner};
