# Official Battlesnake brand marks (DEV-1990)

Vendored official artwork for the site header and favicon set. Do NOT edit
these files — they are the trademarked brand marks, kept byte-identical to
their sources (sha256-pinned below). Themed variants are derived at runtime:

- `logo-light.svg` / `logo-dark.svg` — the "BATTLE SNAKE" wordmark
  (viewBox 710×260). The two files are structurally identical except for one
  fill: light uses `.cls-2{fill:#1c1e21;}`, dark `.cls-2{fill:#ffffff;}`.
  The page header (`server/src/components/page.rs`) inlines `logo-light.svg`
  with those two class fills swapped to `var(--ink)` / `var(--pink)` so the
  mark follows the site/theater theme toggle.
- `icon.svg` — the circle snake-head icon (viewBox 16×16). The header's
  narrow-viewport fallback badge is this file with its `<style>` stripped
  into `fill="var(--pink)"` presentation attributes; `favicon.svg`,
  `favicon.ico`, and `apple-touch-icon.png` at the static root are
  byte-copies of the official favicon set (no theme context in browser
  chrome).

Sources (fetched 2026-10-31):

| file | source | sha256 |
|---|---|---|
| `logo-light.svg` | https://github.com/BattlesnakeOfficial/docs `main`, `static/img/logo-light.svg` | `1cc6c41beaec18778dea9029c61a7b4050e7e548c44a0dd1dc6fd2505d1574ac` |
| `logo-dark.svg` | https://github.com/BattlesnakeOfficial/docs `main`, `static/img/logo-dark.svg` | `5dd122116f9b34a4747a8cd7dcc002ea36893f9bb6aef42a0db9cee3e2d198eb` |
| `icon.svg` | https://static.battlesnake.com/play/releases/2.1.26/ui/icon.svg | `fc911df33308b5e65082bd944574036bc54623624e01adc72bae46ca3dbeb253` |
| `favicon.ico` | https://static.battlesnake.com/play/releases/2.1.26/ui/favicon.ico | `bb0f543e19520dce937bdfc11b9051738ea931038367ac102a36aeddc2619703` |
| `apple-touch-icon.png` | https://static.battlesnake.com/play/releases/2.1.26/ui/apple-touch-icon.png | `e16850cdf8c577bd0a33cadf8318f45557405bc6500d45ea767c20ddcf94d93a` |

If both upstream sources die (Play's static bucket is not guaranteed to
survive the play.battlesnake.com sunset), these vendored files are the
source of truth.

Regenerate (from the repo root):

```bash
curl -sfL -o server/static/brand/logo-light.svg \
  https://raw.githubusercontent.com/BattlesnakeOfficial/docs/main/static/img/logo-light.svg
curl -sfL -o server/static/brand/logo-dark.svg \
  https://raw.githubusercontent.com/BattlesnakeOfficial/docs/main/static/img/logo-dark.svg
curl -sfL -o server/static/brand/icon.svg \
  https://static.battlesnake.com/play/releases/2.1.26/ui/icon.svg
cp server/static/brand/icon.svg server/static/favicon.svg
curl -sfL -o server/static/favicon.ico \
  https://static.battlesnake.com/play/releases/2.1.26/ui/favicon.ico
curl -sfL -o server/static/apple-touch-icon.png \
  https://static.battlesnake.com/play/releases/2.1.26/ui/apple-touch-icon.png
sha256sum server/static/brand/*.svg server/static/favicon.* server/static/apple-touch-icon.png
```

The wordmark fills swap at runtime (`page.rs`), so any upstream change to
`.cls-1/.cls-2/.cls-3` structure fails the `wordmark_is_official_artwork_*`
unit tests loudly rather than rendering a mis-colored trademark.
