use std::{collections::HashMap, sync::LazyLock};

use axum::{
    extract::Path,
    http::{StatusCode, header},
    response::IntoResponse,
};
use include_dir::{Dir, File, include_dir};
use mime_guess::from_path;

// Include the static directory in the binary
static STATIC_DIR: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/static");

// Content hash per embedded file, in subdirectories too (e.g. `design-kit/`).
// /static/* is served with a 1-year Cache-Control, so every reference must
// carry a version that changes with the file contents.
static ASSET_VERSIONS: LazyLock<HashMap<&'static str, String>> = LazyLock::new(|| {
    all_files(&STATIC_DIR)
        .into_iter()
        .filter_map(|file| {
            let path = file.path().to_str()?;
            Some((path, format!("{:016x}", fnv1a(file.contents()))))
        })
        .collect()
});

/// Every file under `dir`, at any depth. Paths stay relative to the static root.
fn all_files<'a>(dir: &'a Dir<'a>) -> Vec<&'a File<'a>> {
    dir.files().chain(dir.dirs().flat_map(all_files)).collect()
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// URL for an embedded static asset, with a content-hash version query
/// so long-lived caches invalidate when the file changes.
pub fn asset_url(path: &str) -> String {
    match ASSET_VERSIONS.get(path) {
        Some(version) => format!("/static/{path}?v={version}"),
        None => format!("/static/{path}"),
    }
}

// Serve static files from the embedded directory
pub async fn serve_static_file(Path(path): Path<String>) -> impl IntoResponse {
    // Try to find the file in the embedded directory
    if let Some(file) = STATIC_DIR.get_file(&path) {
        // Borrowed from the binary, not copied: the design kit's PSDs are about 1 MB
        // each, and a slow download would otherwise hold a copy the whole time.
        let contents: &'static [u8] = file.contents();

        // Guess the MIME type
        let mime_type = from_path(&path).first_or_octet_stream().to_string();

        // Create the response with headers
        (
            [
                (header::CONTENT_TYPE, mime_type),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=31536000".to_string(),
                ),
            ],
            contents,
        )
            .into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// Serve `/favicon.ico` from the embedded SVG. Browsers request this path by
/// convention (before parsing the `<link rel="icon">`), so answering here
/// stops the 404 that otherwise appears in the console on every page. Modern
/// browsers render an SVG served with an image/svg+xml content-type fine.
pub async fn serve_favicon() -> impl IntoResponse {
    match STATIC_DIR.get_file("favicon.svg") {
        Some(file) => (
            [
                (header::CONTENT_TYPE, "image/svg+xml".to_string()),
                (
                    header::CACHE_CONTROL,
                    "public, max-age=31536000".to_string(),
                ),
            ],
            file.contents(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_assets_are_versioned_too() {
        for path in ["arena.css", "design-kit/battlesnake-head-template.psd"] {
            let url = asset_url(path);
            let version = url
                .strip_prefix(&format!("/static/{path}?v="))
                .unwrap_or_else(|| panic!("{url} is not versioned"));
            assert_eq!(version.len(), 16, "{url}");
        }
        assert_eq!(asset_url("missing.css"), "/static/missing.css");
    }
}
