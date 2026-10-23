# Country flag SVGs

Flag badges for leaderboard/ranking/profile country display (DEV-1897).

- Source: [flag-icons](https://github.com/lipis/flag-icons) v7.6.0, `flags/4x3` set.
- License: MIT — see `FLAG-ICONS-LICENSE`.
- Only the 249 ISO-3166-1 alpha-2 country flags are vendored. The upstream
  4x3 set has 271 files; extras like `eu`, `gb-eng`, `gb-sct`, `gb-wls`,
  `gb-nir`, and `un` are intentionally NOT copied — `users.country` stores
  ISO alpha-2 codes only.
- File name = lowercase alpha-2 code (`ca.svg`, `gb.svg`, …), matching the
  logical name `flags/{code}.svg` in the hashed-asset manifest.

Regenerate (from the repo root):

```bash
cd /tmp && curl -sL -o flagicons.tgz \
  https://github.com/lipis/flag-icons/archive/refs/tags/v7.6.0.tar.gz
mkdir -p fi && tar xzf flagicons.tgz -C fi
while read -r code; do
  cp "fi/flag-icons-7.6.0/flags/4x3/$code.svg" server/static/flags/$code.svg
done < <(grep -oP 'code: "\K[A-Z]{2}' server/src/countries/data.rs | tr 'A-Z' 'a-z' | sort -u)
```
