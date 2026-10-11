use std::error::Error;

use vergen::EmitBuilder;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=migrations");
    println!("cargo:rerun-if-changed=static");

    cja_build::AssetsBuilder::new()
        .static_dir("static")
        .frontend_dir("frontend")
        .entry(cja_build::Entry::new("src/theme.ts", "theme.js"))
        .entry(cja_build::Entry::new(
            "src/view-transition.ts",
            "viewTransition.js",
        ))
        .entry(cja_build::Entry::new("src/studio.ts", "studio.js"))
        .entry(cja_build::Entry::new("src/turn-json.ts", "turn-json.js"))
        .entry(cja_build::Entry::new(
            "src/theme-bootstrap.ts",
            "theme-bootstrap.js",
        ))
        .entry(cja_build::Entry::new("src/share-copy.ts", "share-copy.js"))
        .entry(cja_build::Entry::new(
            "src/board-theme-sync.ts",
            "board-theme-sync.js",
        ))
        .entry(cja_build::Entry::new(
            "src/game-status.ts",
            "game-status.js",
        ))
        .entry(cja_build::Entry::new(
            "src/board-resize.ts",
            "board-resize.js",
        ))
        .entry(cja_build::Entry::new(
            "src/live-page-refresh.ts",
            "live-page-refresh.js",
        ))
        .entry(cja_build::Entry::new(
            "src/url-normalization.ts",
            "url-normalization.js",
        ))
        .entry(cja_build::Entry::new(
            "src/game-create-settings.ts",
            "game-create-settings.js",
        ))
        .entry(cja_build::Entry::new("src/ui-actions.ts", "ui-actions.js"))
        .entry(cja_build::Entry::new(
            "src/notifications.ts",
            "notifications.js",
        ))
        .build()?;

    EmitBuilder::builder()
        .all_build()
        .all_cargo()
        .all_git()
        .all_rustc()
        .emit()?;

    Ok(())
}
