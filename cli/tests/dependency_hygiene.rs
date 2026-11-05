//! battlesnake's install story: a snake developer must be able to
//! `cargo install --locked --path cli` without the server's dependency tree.

use std::process::Command;

/// Crates that must never appear in the CLI's normal/build dependency tree.
/// openssl-sys/native-tls would break DEV-2149's static, cross-compiling
/// release binaries; the rest are server-only weight.
const FORBIDDEN: &[&str] = &[
    "sqlx",
    "cja",
    "cja-secret",
    "rusqlite",
    "tiny-skia",
    "usvg",
    "resvg",
    "google-cloud-storage",
    "maud",
    "openssl-sys",
    "native-tls",
];

fn cargo() -> Command {
    let mut cmd = Command::new(env!("CARGO"));
    cmd.current_dir(env!("CARGO_MANIFEST_DIR")); // cargo finds the workspace root
    cmd
}

#[test]
fn normal_dependency_tree_contains_no_server_crates() {
    let out = cargo()
        .args([
            "tree",
            "-p",
            "battlesnake",
            "-e",
            "normal,build",
            "--target",
            "all",
            "--prefix",
            "none",
        ])
        .output()
        .expect("spawn cargo tree");
    assert!(
        out.status.success(),
        "cargo tree failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Each line is `name vX.Y.Z ...` (path or "(*)" suffix); name = first token.
    let names: Vec<&str> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.split_whitespace().next().expect("non-empty line"))
        .collect();
    assert!(
        names.contains(&"arena-api-scope"),
        "tree parse sanity: {names:?}"
    );
    let hits: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| FORBIDDEN.contains(n))
        .collect();
    assert!(
        hits.is_empty(),
        "battlesnake's normal/build dependency tree contains server crates {hits:?}.\n\
         The CLI must stay installable without the server — move shared types to \
         arena-api-scope (no `sqlx` feature) instead of depending on `arena`."
    );
}

#[test]
fn builds_without_workspace_feature_unification() {
    // CI's `cargo test --workspace` unifies features across members (the
    // server enables arena-api-scope's `sqlx` feature), so the cfg-off path
    // this guards is never compiled by that lane. `cargo install --path cli`
    // compiles exactly this path.
    let out = cargo()
        .args(["check", "-p", "battlesnake", "--offline"])
        .output()
        .expect("spawn cargo check");
    assert!(
        out.status.success(),
        "cargo check -p battlesnake failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
