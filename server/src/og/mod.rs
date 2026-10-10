//! Social cards: the 1200×630 PNGs behind each page's `og:image`.
//!
//! Drawn in-process with tiny-skia, the rasteriser the design kit already
//! uses, from layouts in [`cards`]. Every input is server data (names, colours,
//! board frames) and the head and tail art is the design kit's fixed reference
//! table, so nothing user-uploaded is ever parsed here.

pub mod board;
pub mod canvas;
pub mod cards;
pub mod text;
