# The engine host (engine.battlesnake.com)

Arena serves Play's legacy engine public read API on engine hostnames so the
board viewer (whose default engine is `https://engine.battlesnake.com`), the
GIF exporter, old Play links, and third-party tools keep working when Play's
Go engine is shut down. Consumers change nothing at cutover.

## What the engine host serves

Only the engine's public read endpoints, at the engine's own paths:

| Path | Response |
|---|---|
| `GET /games/{id}` | The engine `Game` + `LastFrame` JSON — byte-identical to arena's `/api/games/{id}` |
| `GET /games/{id}/frames` | The engine frames envelope — identical to `/api/games/{id}/frames` |
| `GET /games/{id}/events` | The frames websocket (101, no redirect) |
| `GET /version` | Arena build SHA as `text/plain` |
| `GET /healthz/alive` | `alive` |
| `GET /healthz/ready` | `ready` |

**Everything else 404s with an empty body** — arena's site, cookies, and
OAuth are never reachable under the engine hostname. Games resolve by both
arena ids and archived Play engine ids (DEV-1979).

Host dispatch happens in `cja::server::App`, **before route matching** —
`Router::layer` middleware runs after routing, which would be too late (and
an arena-side `fallback_service` wrap would blank `http.route` span labels).
cja applies its TraceLayer + CookieManagerLayer to each host branch, so
`http.route` stays populated on every host; it is `""` only for the
engine-host 404 fallback.

## `ENGINE_API_HOSTS`

Env var, comma-separated, parsed once at boot in `AppConfig`:

- **Unset or empty** → default `engine.battlesnake.com` (house rule:
  empty = unset).
- **`ENGINE_API_HOSTS=disabled.invalid`** → host routing off (`.invalid` is
  a reserved TLD that can never arrive as a real Host; all-separator values
  like `" , "` are an equally dead list).
- Host matching is cja's: Host header first (URI authority fallback for
  HTTP/2 `:authority`), port stripped, IPv6 brackets handled, ASCII
  case-insensitive, trailing `.` ignored; no match or no host → the default
  (site) router.

## Status mapping

The engine has exactly four status strings; arena's are lossless and
additive (`ArenaStatus` field):

| Arena (`GameStatus`) | Engine `Status` |
|---|---|
| waiting | `stopped` |
| running | `running` |
| finished | `complete` |
| failed | `error` |

These strings are serve-time only — never written to SQL or stored anywhere.

## Ops

The default ships on merge: Cloud Run's deploy only updates the image, and
env is untracked manual config, so zero Terraform/gcloud steps are needed.
To change or disable later:

```
gcloud run services update tf-arena --region us-west1 \
  --update-env-vars ENGINE_API_HOSTS=<hosts-or-disabled.invalid>
```

Use `--update-env-vars` (merges) — **never `--set-env-vars`** (it wipes
`DATABASE_URL` and friends). The VM's default gcloud identity is read-only,
so this is a Corey step.

## Verification pre-cutover

The run.app frontend routes on Host itself, so `curl -H 'Host:
engine.battlesnake.com' https://<run.app>` does **not** reach arena. Pin the
load balancer address instead (136.110.251.85 is what arena.battlesnake.com
resolves to today; cert mismatch is expected until the TLS cutover):

```
curl -sk --resolve engine.battlesnake.com:443:136.110.251.85 \
  https://engine.battlesnake.com/version
```

## Notes

- **TLS/DNS cutover** is DEV-1989; this repo's work ends at host routing.
- **Triage impact**: the old "arena 404s but engine.battlesnake.com answers
  → Play game" recipe breaks after the DNS cutover — the engine host will
  answer for arena-native ids too. Post-cutover discrimination is by
  response content (`ArenaStatus` present = arena data; archived Play games
  resolve by Play id), not by which host answers.
