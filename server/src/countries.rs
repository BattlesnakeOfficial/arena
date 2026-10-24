//! ISO-3166-1 alpha-2 country data and helpers.
//!
//! `users.country` stores only `''` or an alpha-2 code (enforced at the
//! form boundary by [`is_valid_code`], backstopped by a DB CHECK on shape).
//! The generated tables live in [`data`]; this module is the hand-written
//! API over them.

mod data;

pub use data::{ALIASES, COUNTRIES, CSV_NAMES};

/// Map free-text country input to a canonical alpha-2 code. Accepts
/// (case-insensitively, after trimming) alpha-2 codes, ISO CSV names,
/// display names, and aliases — the same match set as the migration's
/// `norm` VALUES. None for empty/unrecognized input. Used at play-claim
/// time; the profile form does NOT use this (it posts codes only).
pub fn normalize(input: &str) -> Option<&'static str> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let lower = input.to_lowercase();
    if let Some(country) = COUNTRIES
        .iter()
        .find(|c| c.code.eq_ignore_ascii_case(input))
    {
        return Some(country.code);
    }
    if let Some((_, code)) = CSV_NAMES.iter().find(|(name, _)| *name == lower) {
        return Some(code);
    }
    if let Some(country) = COUNTRIES.iter().find(|c| c.name.to_lowercase() == lower) {
        return Some(country.code);
    }
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == lower)
        .map(|(_, code)| *code)
}

/// True when input (trimmed) is '' or a known alpha-2 code,
/// case-insensitively. The profile-form boundary check: names and
/// aliases are NOT accepted here.
pub fn is_valid_code(input: &str) -> bool {
    let input = input.trim();
    input.is_empty() || COUNTRIES.iter().any(|c| c.code.eq_ignore_ascii_case(input))
}

/// Display name for an alpha-2 code (case-insensitive, trims).
/// None when unknown — components::flag uses this as its
/// asset-membership guard.
pub fn name_for(code: &str) -> Option<&'static str> {
    let code = code.trim();
    COUNTRIES
        .iter()
        .find(|c| c.code.eq_ignore_ascii_case(code))
        .map(|c| c.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn normalize_passes_codes_through_canonicalized() {
        assert_eq!(normalize("CA"), Some("CA"));
        assert_eq!(normalize("ca"), Some("CA"));
        assert_eq!(normalize(" cA "), Some("CA"));
    }

    #[test]
    fn normalize_maps_names() {
        assert_eq!(normalize("Canada"), Some("CA"));
        assert_eq!(normalize("united kingdom"), Some("GB"));
        assert_eq!(normalize("Türkiye"), Some("TR"));
        assert_eq!(
            normalize("United Kingdom of Great Britain and Northern Ireland"),
            Some("GB")
        );
    }

    #[test]
    fn normalize_maps_aliases() {
        assert_eq!(normalize("USA"), Some("US"));
        assert_eq!(normalize("UK"), Some("GB"));
        assert_eq!(normalize("Holland"), Some("NL"));
        assert_eq!(normalize("DRC"), Some("CD"));
        assert_eq!(normalize("Côte d'Ivoire"), Some("CI"));
        assert_eq!(normalize("ivory coast"), Some("CI"));
    }

    #[test]
    fn normalize_rejects_empty_and_junk() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("  "), None);
        assert_eq!(normalize("Narnia"), None);
        assert_eq!(normalize("XX"), None);
    }

    #[test]
    fn country_table_is_well_formed() {
        assert_eq!(COUNTRIES.len(), 249);
        let mut seen = HashSet::new();
        for c in COUNTRIES {
            let code = c.code;
            assert_eq!(code.len(), 2, "code length: {code}");
            assert!(
                code.chars().all(|ch| ch.is_ascii_uppercase()),
                "not upper alpha: {code}"
            );
            assert!(seen.insert(code), "duplicate code: {code}");
        }
        let names: Vec<(String, &str)> = COUNTRIES
            .iter()
            .map(|c| (c.name.to_lowercase(), c.code))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "COUNTRIES must be sorted by display name");
        for (name, code) in CSV_NAMES {
            assert!(seen.contains(code), "CSV name {name} -> unknown {code}");
        }
        for (alias, code) in ALIASES {
            assert!(seen.contains(code), "alias {alias} -> unknown {code}");
        }
    }

    #[test]
    fn is_valid_code_accepts_codes_only() {
        assert!(is_valid_code(""));
        assert!(is_valid_code("CA"));
        assert!(is_valid_code("ca"));
        assert!(!is_valid_code("Canada"));
        assert!(!is_valid_code("UK"));
        assert!(!is_valid_code("XX"));
    }

    #[test]
    fn name_for_looks_up_codes() {
        assert_eq!(name_for("CA"), Some("Canada"));
        assert_eq!(name_for("ca"), Some("Canada"));
        assert_eq!(name_for(""), None);
        assert_eq!(name_for("XX"), None);
    }

    /// Every country code must have a vendored flag asset: `asset_url`
    /// panics on unknown logical names, so a missing file here would mean
    /// a production panic, not a missing image. `country_flag` guards
    /// with `name_for`, and this test guarantees the manifest side.
    #[test]
    fn every_country_has_a_vendored_flag_asset() {
        let manifest = &crate::arena_assets::MANIFEST;
        for c in COUNTRIES {
            let logical = format!("flags/{}.svg", c.code.to_lowercase());
            assert!(
                manifest.entries.iter().any(|a| a.logical_name == logical),
                "manifest missing {logical}"
            );
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("static")
                .join(&logical);
            assert!(path.exists(), "static file missing: {}", path.display());
        }
    }
}
