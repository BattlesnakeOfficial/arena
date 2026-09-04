use maud::{Markup, PreEscaped, html};

pub(crate) const LIVE_PAGE_REFRESH_JS: &str = r#"(() => {
  const script = document.currentScript;
  const fallback = script.nextElementSibling;
  const revealFallback = () => { fallback.hidden = false; };
  const intervalMs = Number(script.dataset.intervalMs);
  const maxTicks = Number(script.dataset.maxTicks);
  const storageKey = `arena:live-refresh:${location.pathname}${location.search}:${script.dataset.stateKey}`;

  const schedule = () => {
    if (document.hidden) {
      document.addEventListener('visibilitychange', () => {
        if (!document.hidden) schedule();
      }, { once: true });
      return;
    }

    let ticks;
    try {
      const stored = sessionStorage.getItem(storageKey);
      ticks = stored === null ? 0 : Number.parseInt(stored, 10);
      if (!Number.isSafeInteger(ticks) || ticks < 0) ticks = 0;
    } catch (_) {
      revealFallback();
      return;
    }

    if (ticks >= maxTicks) {
      revealFallback();
      return;
    }

    try {
      sessionStorage.setItem(storageKey, String(ticks + 1));
    } catch (_) {
      revealFallback();
      return;
    }
    setTimeout(() => location.reload(), intervalMs);
  };

  schedule();
})();"#;

pub(crate) struct LiveRefreshConfig<'a> {
    pub interval_ms: u64,
    pub max_ticks: u32,
    pub state_key: &'a str,
}

pub(crate) fn live_page_refresh(config: &LiveRefreshConfig<'_>) -> Markup {
    html! {
        script data-live-page-refresh
            data-interval-ms=(config.interval_ms)
            data-max-ticks=(config.max_ticks)
            data-state-key=(config.state_key) {
            (PreEscaped(LIVE_PAGE_REFRESH_JS))
        }
        div data-live-page-refresh-expired hidden {
            "Automatic refresh stopped. "
            a href="" { "Refresh" }
        }
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

        let script_body = markup
            .split_once('>')
            .unwrap()
            .1
            .split_once("</script>")
            .unwrap()
            .0;
        assert_eq!(script_body, LIVE_PAGE_REFRESH_JS);
        assert!(markup.contains("data-live-page-refresh"));
        assert!(markup.contains("data-live-page-refresh-expired"));
        assert!(markup.contains("data-interval-ms=\"5000\""));
        assert!(markup.contains("data-max-ticks=\"120\""));
        assert!(markup.contains("state&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(!script_body.contains("alert(1)"));
        assert!(!script_body.contains("5000"));
    }
}
