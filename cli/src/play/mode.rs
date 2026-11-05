//! Resolving `-g`/`-m` flags to an arena game mode, plus `play`'s input
//! validation (board size, snake count, name/URL pairing, numeric flags).
//!
//! Every rejection says the combination "isn't available yet" and lists the
//! valid values — the Go CLI silently fell back to standard; this CLI (which
//! runs arena's five modes, not Go's full surface) refuses instead.

use color_eyre::eyre::{Result, eyre};

use crate::play::SnakeSpec;

/// A resolved game: the internal engine mode plus the public wire pair.
///
/// The wire pair is carried separately from the mode because requests send
/// what the *user asked for*, not what the mode maps to: `-g royale -m royale`
/// and `-g standard -m royale` are both Royale internally, but they send
/// different `ruleset.name` values (see `resolve_mode`'s table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGame {
    pub mode: engine_core::GameMode,
    pub wire_ruleset: String,
    pub wire_map: String,
}

const VALID_GAMETYPES: &str = "standard, solo, royale, constrictor, snail_mode";
const VALID_MAPS: &str = "standard, royale, empty, snail_mode";
const VALID_SIZES: &str = "7x7, 11x11, 19x19";

/// Resolve the `-g`/`-m` flag pair to a mode + wire pair.
///
/// Accepted combinations (anything else is an error — never a silent
/// standard fallback like the Go CLI):
///
/// | `-g` | `-m` | mode | wire ruleset | wire map |
/// |---|---|---|---|---|
/// | `standard` (default) | `standard` (default) | Standard | `standard` | `standard` |
/// | `solo` | omitted / `standard` | Solo | `solo` | `standard` |
/// | `royale` | omitted / `royale` | Royale | `royale` | `royale` |
/// | `standard` | `royale` | Royale | `standard` | `royale` |
/// | `constrictor` | omitted / `empty` | Constrictor | `constrictor` | `empty` |
/// | `standard` | `snail_mode` | SnailMode | `standard` | `snail_mode` |
/// | `snail_mode` | omitted / `snail_mode` | SnailMode | `standard` | `snail_mode` |
pub fn resolve_mode(gametype: &str, map: Option<&str>) -> Result<ResolvedGame> {
    let unknown_gametype = |g: &str| {
        eyre!("game type '{g}' isn't available yet (valid game types: {VALID_GAMETYPES})")
    };
    let unknown_map = |m: &str| eyre!("map '{m}' isn't available yet (valid maps: {VALID_MAPS})");
    let invalid_pair = |m: &str| {
        eyre!(
            "game type '{gametype}' with map '{m}' isn't available yet \
             (valid game types: {VALID_GAMETYPES}; valid maps: {VALID_MAPS})"
        )
    };

    let resolved = match (gametype, map) {
        ("standard", None) => ResolvedGame {
            mode: engine_core::GameMode::Standard,
            wire_ruleset: "standard".into(),
            wire_map: "standard".into(),
        },
        ("standard", Some("standard")) => ResolvedGame {
            mode: engine_core::GameMode::Standard,
            wire_ruleset: "standard".into(),
            wire_map: "standard".into(),
        },
        // The classic combination the starter READMEs use: `-g solo` with no
        // map (or the default map).
        ("solo", None) | ("solo", Some("standard")) => ResolvedGame {
            mode: engine_core::GameMode::Solo,
            wire_ruleset: "solo".into(),
            wire_map: "standard".into(),
        },
        // Arena's Royale always has hazards. The Go CLI ran royale without
        // hazards here because Go derives hazards from the map (its `-m`
        // defaults to "standard"); this CLI deliberately diverges.
        ("royale", None) | ("royale", Some("royale")) => ResolvedGame {
            mode: engine_core::GameMode::Royale,
            wire_ruleset: "royale".into(),
            wire_map: "royale".into(),
        },
        // Go-syntax Royale: the royale map on the standard ruleset.
        ("standard", Some("royale")) => ResolvedGame {
            mode: engine_core::GameMode::Royale,
            wire_ruleset: "standard".into(),
            wire_map: "royale".into(),
        },
        ("constrictor", None) | ("constrictor", Some("empty")) => ResolvedGame {
            mode: engine_core::GameMode::Constrictor,
            wire_ruleset: "constrictor".into(),
            wire_map: "empty".into(),
        },
        // Snail Mode was a community map on the standard ruleset, so it
        // announces itself as exactly that — both here and via -g snail_mode.
        ("snail_mode", None) | ("snail_mode", Some("snail_mode")) => ResolvedGame {
            mode: engine_core::GameMode::SnailMode,
            wire_ruleset: "standard".into(),
            wire_map: "snail_mode".into(),
        },
        ("standard", Some("snail_mode")) => ResolvedGame {
            mode: engine_core::GameMode::SnailMode,
            wire_ruleset: "standard".into(),
            wire_map: "snail_mode".into(),
        },
        (g, Some(m))
            if ["standard", "solo", "royale", "constrictor", "snail_mode"].contains(&g) =>
        {
            return Err(invalid_pair(m));
        }
        (_, Some(m)) if !["standard", "royale", "empty", "snail_mode"].contains(&m) => {
            return Err(unknown_map(m));
        }
        (g, _) => return Err(unknown_gametype(g)),
    };
    Ok(resolved)
}

/// Validate a board size: arena runs 7x7, 11x11 and 19x19, square only.
pub fn validate_board_size(width: i32, height: i32) -> Result<()> {
    if width == height && [7, 11, 19].contains(&width) {
        Ok(())
    } else {
        Err(eyre!(
            "board size {width}x{height} isn't available yet (valid sizes: {VALID_SIZES})"
        ))
    }
}

/// Validate a resolved mode's snake count. Arena's limits: at least one
/// snake, `place_snakes_fixed` handles at most 8, Solo takes exactly one.
pub fn validate_snake_count(resolved: &ResolvedGame, count: usize) -> Result<()> {
    match (count, resolved.mode) {
        (0, _) => Err(eyre!("At least one battlesnake is required for a game")),
        (1, _) => Ok(()),
        (n, engine_core::GameMode::Solo) if n > 1 => {
            Err(eyre!("Solo games require exactly one battlesnake"))
        }
        (n, _) if n > 8 => Err(eyre!(
            "{n} snakes isn't available yet (games support 1 to 8 snakes)"
        )),
        _ => Ok(()),
    }
}

/// Pair `-n`/`-u` lists by position. A name with no URL is Go's error; a URL
/// with no name draws from the generated-name pool (see `names.rs`).
///
/// `namer` is injected so tests don't drain the process-global pool.
pub fn pair_snakes(
    names: &[String],
    urls: &[String],
    mut namer: impl FnMut() -> String,
) -> Result<Vec<SnakeSpec>> {
    if names.len() > urls.len() {
        // Go reports the first name without a URL.
        return Err(eyre!(
            "URL for name {} is missing",
            names[urls.len().min(names.len() - 1)]
        ));
    }
    let mut snakes = Vec::with_capacity(urls.len());
    for (i, url) in urls.iter().enumerate() {
        let name = match names.get(i) {
            Some(name) => name.clone(),
            None => namer(),
        };
        snakes.push(SnakeSpec {
            name,
            url: url.clone(),
        });
    }
    Ok(snakes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine_core::GameMode;

    fn resolve(g: &str, m: Option<&str>) -> ResolvedGame {
        resolve_mode(g, m).unwrap()
    }

    /// All 10 accepted (-g, -m) combinations and their wire pairs.
    #[test]
    fn accepted_combinations_resolve_with_the_right_wire_pair() {
        let expected = [
            ("standard", None, GameMode::Standard, "standard", "standard"),
            (
                "standard",
                Some("standard"),
                GameMode::Standard,
                "standard",
                "standard",
            ),
            ("solo", None, GameMode::Solo, "solo", "standard"),
            ("solo", Some("standard"), GameMode::Solo, "solo", "standard"),
            ("royale", None, GameMode::Royale, "royale", "royale"),
            (
                "royale",
                Some("royale"),
                GameMode::Royale,
                "royale",
                "royale",
            ),
            (
                "standard",
                Some("royale"),
                GameMode::Royale,
                "standard",
                "royale",
            ),
            (
                "constrictor",
                None,
                GameMode::Constrictor,
                "constrictor",
                "empty",
            ),
            (
                "constrictor",
                Some("empty"),
                GameMode::Constrictor,
                "constrictor",
                "empty",
            ),
            (
                "snail_mode",
                None,
                GameMode::SnailMode,
                "standard",
                "snail_mode",
            ),
            (
                "snail_mode",
                Some("snail_mode"),
                GameMode::SnailMode,
                "standard",
                "snail_mode",
            ),
            (
                "standard",
                Some("snail_mode"),
                GameMode::SnailMode,
                "standard",
                "snail_mode",
            ),
        ];
        // 12 rows: the table in the plan counts `-m` omitted and the row
        // default as separate accepted inputs.
        assert_eq!(expected.len(), 12);
        for (g, m, mode, wire_ruleset, wire_map) in expected {
            let r = resolve(g, m);
            assert_eq!(r.mode, mode, "{g} {m:?}");
            assert_eq!(r.wire_ruleset, wire_ruleset, "{g} {m:?}");
            assert_eq!(r.wire_map, wire_map, "{g} {m:?}");
        }
    }

    #[test]
    fn invalid_pairs_are_rejected_with_the_full_message() {
        for (g, m) in [
            ("royale", "standard"),
            ("solo", "royale"),
            ("constrictor", "standard"),
            ("snail_mode", "royale"),
            ("solo", "empty"),
        ] {
            let err = resolve_mode(g, Some(m)).unwrap_err().to_string();
            assert!(
                err.contains(&format!(
                    "game type '{g}' with map '{m}' isn't available yet"
                )),
                "{g} {m}: {err}"
            );
            assert!(
                err.contains("valid game types: standard, solo, royale, constrictor, snail_mode"),
                "{g} {m}: {err}"
            );
            assert!(
                err.contains("valid maps: standard, royale, empty, snail_mode"),
                "{g} {m}: {err}"
            );
        }
    }

    /// Go game types arena doesn't run hit the unknown-value errors — never
    /// a silent standard fallback.
    #[test]
    fn unrun_go_gametypes_and_maps_are_rejected() {
        for g in ["wrapped", "wrapped_constrictor", "hz_rings", "squad"] {
            let err = resolve_mode(g, None).unwrap_err().to_string();
            assert!(
                err.contains(&format!("game type '{g}' isn't available yet")),
                "{g}: {err}"
            );
            assert!(
                err.contains("valid game types: standard, solo, royale, constrictor, snail_mode"),
                "{g}: {err}"
            );
        }
        for m in ["hz_rings", "wrapped", "wrapped_constrictor", "islands"] {
            let err = resolve_mode("standard", Some(m)).unwrap_err().to_string();
            assert!(
                err.contains(&format!("map '{m}' isn't available yet")),
                "{m}: {err}"
            );
            assert!(
                err.contains("valid maps: standard, royale, empty, snail_mode"),
                "{m}: {err}"
            );
        }
    }

    #[test]
    fn board_sizes() {
        for s in [7, 11, 19] {
            assert!(validate_board_size(s, s).is_ok(), "{s}x{s}");
        }
        for (w, h) in [
            (7, 19),
            (19, 7),
            (9, 9),
            (25, 25),
            (3, 3),
            (11, 9),
            (-11, -11),
            (0, 0),
        ] {
            let err = validate_board_size(w, h).unwrap_err().to_string();
            assert!(
                err.contains(&format!("board size {w}x{h} isn't available yet")),
                "{w}x{h}: {err}"
            );
            assert!(
                err.contains("valid sizes: 7x7, 11x11, 19x19"),
                "{w}x{h}: {err}"
            );
        }
    }

    #[test]
    fn snake_counts() {
        let std = resolve("standard", None);
        let solo = resolve("solo", None);
        assert_eq!(
            validate_snake_count(&std, 0).unwrap_err().to_string(),
            "At least one battlesnake is required for a game"
        );
        assert!(validate_snake_count(&std, 1).is_ok());
        assert!(validate_snake_count(&std, 8).is_ok());
        let err = validate_snake_count(&std, 9).unwrap_err().to_string();
        assert_eq!(
            err,
            "9 snakes isn't available yet (games support 1 to 8 snakes)"
        );
        assert_eq!(
            validate_snake_count(&solo, 2).unwrap_err().to_string(),
            "Solo games require exactly one battlesnake"
        );
        assert!(validate_snake_count(&solo, 1).is_ok());
    }

    #[test]
    fn names_and_urls_pair_by_position() {
        let mut generated = 0usize;
        let mut namer = || {
            generated += 1;
            format!("Generated {generated}")
        };
        // Equal lists pair positionally.
        let snakes = pair_snakes(
            &["A".to_string(), "B".to_string()],
            &["http://a".to_string(), "http://b".to_string()],
            &mut namer,
        )
        .unwrap();
        assert_eq!(snakes[0].name, "A");
        assert_eq!(snakes[0].url, "http://a");
        assert_eq!(snakes[1].name, "B");
        // More URLs than names: generated names fill the gaps.
        let snakes = pair_snakes(
            &["A".to_string()],
            &["http://a".to_string(), "http://b".to_string()],
            namer,
        )
        .unwrap();
        assert_eq!(snakes[0].name, "A");
        assert_eq!(snakes[1].name, "Generated 1");
        // More names than URLs: Go's error.
        let err = pair_snakes(
            &["A".to_string(), "No URL".to_string()],
            &["http://a".to_string()],
            || "unused".to_string(),
        )
        .unwrap_err()
        .to_string();
        assert_eq!(err, "URL for name No URL is missing");
    }
}
