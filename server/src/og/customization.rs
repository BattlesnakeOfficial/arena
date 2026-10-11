//! Fixed-palette catalog art for email clients that do not render SVG.

use arena::design_kit::{AssetKind, catalog_shapes::CatalogShape};
use color_eyre::eyre::eyre;
use tiny_skia::{Pixmap, Rect, Transform};

use super::{board, canvas};

pub fn render(kind: AssetKind, slug: &str) -> cja::Result<Vec<u8>> {
    let shape = board::catalog_shape(kind, slug).ok_or_else(|| eyre!("Unknown catalog art"))?;
    render_shape(shape)
}

pub(crate) fn render_shape(shape: &CatalogShape) -> cja::Result<Vec<u8>> {
    let path = board::shape_path(shape).ok_or_else(|| eyre!("Invalid catalog path"))?;
    let mut pixmap = Pixmap::new(128, 128).ok_or_else(|| eyre!("Could not allocate art pixmap"))?;
    pixmap.fill(canvas::rgb(0xe7, 0xdf, 0xe3));
    if let Some(inner) = Rect::from_xywh(2.0, 2.0, 124.0, 124.0) {
        pixmap.fill_rect(
            inner,
            &canvas::solid(canvas::rgb(0xfc, 0xfb, 0xf8)),
            Transform::identity(),
            None,
        );
    }
    pixmap.fill_path(
        &path,
        &canvas::solid(canvas::rgb(0xc1, 0x16, 0x55)),
        board::fill_rule(shape.fill_rule),
        board::tile_transform(0.0, 0.0, 128.0),
        None,
    );
    canvas::encode_png(&pixmap)
}
