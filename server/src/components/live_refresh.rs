use maud::{Markup, html};

/// Configuration for a bounded refresh cycle tied to a server-state fingerprint.
pub(crate) struct LiveRefreshConfig<'a> {
    pub interval_ms: u64,
    pub max_ticks: u32,
    pub state_key: &'a str,
}

/// Renders a one-shot live refresh and its manual fallback.
pub(crate) fn live_page_refresh(config: &LiveRefreshConfig<'_>) -> Markup {
    html! {
        div class="live-refresh-expired" data-live-page-refresh-expired hidden {
            "Automatic refresh stopped. "
            a href="" { "Refresh" }
        }
        script src=(crate::arena_assets::asset_url("live-page-refresh.js"))
            data-live-page-refresh
            data-interval-ms=(config.interval_ms)
            data-max-ticks=(config.max_ticks)
            data-state-key=(config.state_key) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_static_script_and_escaped_configuration() {
        let markup = live_page_refresh(&LiveRefreshConfig {
            interval_ms: 5_000,
            max_ticks: 120,
            state_key: r#"state"><script>alert(1)</script>"#,
        })
        .into_string();

        let bundle = crate::arena_assets::MANIFEST
            .entries
            .iter()
            .find(|asset| asset.logical_name == "live-page-refresh.js")
            .expect("compiled live refresh bundle");
        assert!(
            std::str::from_utf8(bundle.bytes)
                .unwrap()
                .contains("currentScript")
        );
        assert!(markup.contains(&crate::arena_assets::asset_url("live-page-refresh.js")));
        assert!(markup.contains("data-live-page-refresh"));
        assert!(markup.contains("data-live-page-refresh-expired"));
        assert!(
            markup.find("data-live-page-refresh-expired").unwrap()
                < markup.find("<script src=").unwrap()
        );
        assert!(markup.contains("data-interval-ms=\"5000\""));
        assert!(markup.contains("data-max-ticks=\"120\""));
        assert!(markup.contains("state&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!markup.contains("defer"));
        assert!(!markup.contains("async"));
    }
}
