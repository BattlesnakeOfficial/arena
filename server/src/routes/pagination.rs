//! Offset-pagination arithmetic shared by the public directories.

/// Resolve a requested (zero-based) page number against a total row count.
/// Returns `(page, total_pages)`. Missing/negative requests clamp to the first
/// page, oversized ones to the last. An empty listing still reports one
/// logical page so the clamp stays in range.
pub fn resolve_page(requested: Option<i64>, total: i64, per_page: i64) -> (i64, i64) {
    let total_pages = if total > 0 {
        (total + per_page - 1) / per_page
    } else {
        1
    };
    let page = requested.unwrap_or(0).clamp(0, total_pages - 1);

    (page, total_pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_listing_is_still_one_page() {
        assert_eq!(resolve_page(None, 0, 50), (0, 1));
        assert_eq!(resolve_page(Some(7), 0, 50), (0, 1));
    }

    #[test]
    fn out_of_range_requests_clamp_to_the_ends() {
        assert_eq!(resolve_page(Some(-3), 120, 50), (0, 3));
        assert_eq!(resolve_page(Some(99), 120, 50), (2, 3));
    }

    #[test]
    fn page_count_rounds_up_to_cover_a_partial_last_page() {
        assert_eq!(resolve_page(None, 50, 50), (0, 1));
        assert_eq!(resolve_page(Some(1), 51, 50), (1, 2));
        assert_eq!(resolve_page(Some(3), 200, 50), (3, 4));

        // Per-page size is a parameter, not baked in.
        assert_eq!(resolve_page(Some(4), 51, 10), (4, 6));
    }
}
