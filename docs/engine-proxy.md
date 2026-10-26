# Engine proxy (Rust, DEV-1943)

The regional engine proxy — the hop arena's `snake_client` calls for
us-east4 / europe-west4 snakes — as a Rust service we own, built on
[cja](https://github.com/coreyja/cja). Replaces the Go proxy
(`conflictserpent/engine proxy_server`, source `BattlesnakeOfficial/engine`
`src/proxy/`). Crate: `engine-proxy/` in this workspace; binary is named
`engine` with a `proxy_server` subcommand, so the infrastructure#51 bootstrap
line runs unchanged:

```
docker run ... --entrypoint engine ${image} proxy_server
```

The Go image resolves `--entrypoint engine` via `/bin/engine`; ours installs
at `/usr/local/bin/engine` (distroless `PATH` includes `/usr/local/bin` —
exercised by the CI smoke and the local bootstrap check).

Design references: DEV-1614 (instance churn dropping in-flight moves — the
per-request log line below is the missing evidence), DEV-1499 item 2
(`MaxIdleConnsPerHost: 2` handshake storms — the pool default here is 8/host),
DEV-1946 (proxy-fault retry).

## Wire protocol

Unchanged from the Go proxy; pinned on both sides by
`server/src/snake_client_contract_tests.rs` (21 cases through arena's real
`request_move_routed` / `request_info_routed` paths) and
`engine-proxy/tests/proxy_contract.rs`. The same arena suite runs against the
Go image with `ENGINE_PROXY_GO_TARGET=docker://<image>` — locally, via
`engine-proxy/scripts/go-contract.sh`; it is not a CI job because the Go
source repo is private with no public image and CI may not gain secrets
(coordinator decision, PR #277 round 1)
to keep the two pinned to one contract during the canary.

### Request (arena → proxy)

Same method and body as the snake call, sent to the proxy base URL with:

| header | behavior |
|---|---|
| `X-Request-URI: <snake url>` | target of the forwarded request; path/query/fragment preserved |
| `X-Battlesnake-Timeout-Ms: <ms>` | hard wall-clock budget (see below); absent ⇒ 1000ms; unparseable ⇒ 400; parsed value clamped to `[0, ENGINE_PROXY_MAX_TIMEOUT_MS]` |
| `X-Proxy-Authorization: token <secret>` | auth; `Proxy-Authorization` accepted as fallback (X- wins even when wrong — Go parity) |

- **Auth is checked first**, before URI/timeout validation (Go order).
- The scheme token must equal `token` **case-sensitively** (Go parity);
  secret compared constant-time (`subtle`).
- Failure ⇒ **407, empty body, no latency header** — arena's
  `classify_proxy_response` keys the direct-retry on exactly that shape.
- Malformed (missing URI, non-http(s) scheme, bad timeout header) ⇒ 400.
- Control headers never reach the snake: both auth headers, `X-Request-URI`,
  `X-Battlesnake-Timeout-Ms`, `X-Forwarded-For`, plus hop-by-hop headers.
- User-Agent is forwarded verbatim; when the caller sends none, **no** UA
  header is sent (matches Go's blank-set suppression).
- Request bodies are buffered, capped at `ENGINE_PROXY_BODY_CAP_BYTES` (413
  when exceeded). Oversized/stalled uploads fall under the same budget: a
  client that sends headers then stalls gets 504 `1:` *before* anything is
  forwarded.

### The budget (one deadline, three phases)

`X-Battlesnake-Timeout-Ms` is one wall-clock deadline computed at handler
entry (after the 407/400 fast paths), covering:

1. **request_body** — buffering the inbound request body;
2. **headers** — connect + request write + snake response headers;
3. **body** — streaming the snake's response body (a `DeadlineBody` wrapper
   aborts the connection past the deadline).

Phase-split consequences (arena-visible):

- **Pre-headers timeout ⇒ 504 + `X-Battlesnake-Latency-Ms` +
  `X-Battlesnake-Server-Error: 1:A response was not received before the timeout`**
  — arena maps this to `SnakeTransportFailure { timed_out: true }` → fallback
  move, `timed_out: true`.
- **After a 200 status line has been forwarded, no 504 can follow** — the
  body-phase abort kills the connection; arena's `read_body_capped` fails
  inside `execute_proxy` ⇒ `ProxyCall::Fault { kind: "transport" }`. Since
  DEV-1946 (#276) arena treats that as a proxy fault and retries the move
  **directly** with the snake's full fresh timeout — the turn carries the
  direct call's real result (its move, latency, and status), never the
  fallback. Pre-DEV-1946 the same abort produced a fallback move with
  `timed_out: false`, `latency_ms: None`; the proxy's behavior is unchanged,
  only arena's recovery differs. Both legs (the abort and the 504 above)
  complete before arena's local guard (`timeout + 300ms` transit margin), so
  the retry still gets a full budget. Contract case 17b pins this end to
  end: the stall snake accepts exactly two connections (proxied, aborted;
  then the direct retry) and the turn ends with the direct move.

### Response (proxy → arena)

- Snake status, headers, and body pass through unchanged (407/500/504 from
  the *snake* keep the latency header and carry no server-error header —
  they are `SnakeResponse`s, not proxy faults).
- `X-Battlesnake-Latency-Ms`: whole ms elapsed at response headers, on
  success *and* 502/504 (never on 407).
- `X-Battlesnake-Timing-Ms`: Go's format
  `dns=%.3f connect=%.3f tls=%.3f client_headers=%.3f client_body=%.3f first_byte=%.3f total=%.3f`;
  dns/connect/tls are zero on pooled reuse (a first-use flag on the pooled
  connection, immune to hyper-util's checkout-vs-dial race). Success/snake
  status responses only.
- On snake transport failure: **504** for timeout-class, **502** otherwise,
  with `X-Battlesnake-Server-Error: "<code>:<message>"` (Go's exact strings):
  `1` timeout, `2` dns, `3` connect, `4` tls, `5` read body, `0` unknown.
- Declared oversized response (`Content-Length` > cap) ⇒ 502 + `5:` *before*
  headers are forwarded — arena gets a classifiable transport failure instead
  of a truncated 200 (deliberate improvement over Go). Streamed/chunked
  bodies past the cap abort the connection (`engine_proxy_body_aborts_total`).

### Other endpoints

- `GET /healthz/alive` → 200 `alive`; 503 only if all `HEALTH_CHECK_URLS`
  fail. `GET /healthz/ready` → 200 `ready`.
- `GET /version` → `APP_VERSION` (`unknown` when unset).
- `GET /metrics` → prometheus text; `Authorization: token <secret>` required
  (401 + `WWW-Authenticate: token` otherwise) — deliberately a different
  header/status from the data path.

## Config (env)

| key | default | meaning |
|---|---|---|
| `PORT` | 8080 | listen port (cja) |
| `BATTLESNAKE_PROXY_AUTH_TOKEN` | *required* | proxy auth secret; absent ⇒ exit 1 (like Go) |
| `APP_VERSION` | `unknown` | served at `/version` |
| `HEALTH_CHECK_URLS` | — | optional comma-separated upstream probes for `/healthz/alive` |
| `ENGINE_PROXY_POOL_IDLE_PER_HOST` | 8 | idle connections per host (Go's bug was 2) |
| `ENGINE_PROXY_POOL_IDLE_TIMEOUT_SECS` | 90 | matches Go's `IdleConnTimeout` |
| `ENGINE_PROXY_BODY_CAP_BYTES` | 1048576 | request *and* response body cap |
| `ENGINE_PROXY_MAX_TIMEOUT_MS` | 10000 | clamp for the client-supplied budget |
| `ENGINE_PROXY_DIAL_TIMEOUT_MS` | 10000 | overall cap on one connector call: DNS + connect race + TLS handshake (raise with MAX_TIMEOUT_MS; 1–3600000, else boot fails) |
| `ENGINE_PROXY_EXTRA_CA` | — | additional CA PEM path (rustls, ALPN h2+http/1.1) |
| `RUST_LOG` | baked (below) | see the log-shape section |
| `JSON_LOGS` | 1 (baked) | JSON stdout logs |
| `CJA_SHUTDOWN_JOB_DRAIN_SECS` / `CJA_SHUTDOWN_EXIT_GRACE_SECS` | 1 / 3 (baked) | drain budget 4s < docker stop's 10s SIGKILL |
| `EYES_ORG_ID` / `EYES_APP_ID` / `EYES_URL` | — | Eyes telemetry wiring (cja layer) |

## Connections

hyper-util legacy client over a hand-rolled timed connector (reqwest exposes
no dns/connect/TLS phase timing, and `X-Battlesnake-Timing-Ms` needs it):

- DNS via `tokio::net::lookup_host`, sorted IPv4-first, then a staggered
  connect race (RFC 8305 style, 250ms stagger — Go uses 300ms DualStack):
  first success wins, losers are dropped. **No per-attempt timeout** — distant
  IPv4-only snakes (RTT ~200-250ms) and lost SYNs (kernel retry ~1s) connect
  fine, as with Go. Attempts are bounded by the request deadline (on-path
  dials) and `ENGINE_PROXY_DIAL_TIMEOUT_MS` (background dials).
- `fresh_connection` in the log line = this request was the first to *use*
  the connection (an `AtomicBool` on the pooled connection). A fresh
  connection with `connect_ms`/`tls_ms` ≈ 0 means a background dial (pool
  checkout-vs-dial race) landed idle and this request picked it up — the
  handshake wasn't on this request's critical path.
- h2 offered via ALPN, matching Go's `ForceAttemptHTTP2`. No pre-warming:
  on EC2 the process is long-lived (the 300-500 swaps/day was Cloud Run
  churn); a deploy costs one handshake per host, absorbed by arena's
  fallback, and measured by the load check's mock-side accept counters.
- Env proxies (`HTTPS_PROXY` etc.) are deliberately ignored — same as the Go
  proxy.

## Logs: exactly one line per request

Every proxied request emits exactly one structured line (INFO, target
`engine_proxy`, message `proxied request`):

```
host=127.0.0.1:33601 scheme=http method=POST outcome=transport:connect status=502
timeout_ms=500 error="tcp connect error: 127.0.0.1:33601 (connection refused)"
dns_ms=0.0 connect_ms=0.0 tls_ms=0.0 client_headers_ms=0.0
client_body_ms=0.0 first_byte_ms=0.0 total_ms=0.2 fresh_connection=false
process_age_secs=8
```

(with `JSON_LOGS=1`, the same fields as JSON — `"outcome":"transport:connect"`, …)

- `host` is `host[:port]` only — userinfo (`user:pass@`) and any
  `?token=`-style query are stripped by `redact_host_or_url` before the
  line is written (snake URLs are user-supplied; pinned by
  `userinfo_credentials_in_snake_url_are_never_logged`).
- `error` carries the redacted transport-failure detail on failed requests
  (empty otherwise) — failed requests stay **one-line events**: the
  standalone failure WARNs are DEBUG-level and dropped by the production
  filter.
- `outcome` ∈ `success`, `timeout:request_body|headers|body`,
  `transport:dns|connect|tls|read_body|unknown`, `auth`, `malformed`,
  `request_body_too_large`, `aborted:<reason>`, `client_cancelled`,
  `shutdown_aborted`, plus the snake status for pass-throughs.
- `client_cancelled` fires from a drop guard when the caller disconnects
  before a terminal outcome (arena's local guard killing the handler mid-await,
  mid-upload, mid-body) — **the evidence class DEV-1614 has been missing**.
  `shutdown_aborted` labels aborts after the Supervisor's shutdown token
  fires. Label caveats: a client that cancels on its own during the ≤4s drain
  window is labelled `shutdown_aborted` (the token never resets; the reverse
  mislabel is impossible); a handler *panic* unwinds through Drop and
  mislabels as `client_cancelled` — the CatchPanicLayer response (500 +
  `X-Battlesnake-Server-Error: 0:An unknown error occurred`) and its
  `tower_http::catch_panic` ERROR line still identify it.
- Hosts are logged host[:port] only — snake URLs can carry `?token=`; all
  URL-bearing strings go through redaction before logging. Auth failures log
  only "bad or missing proxy auth" (the Go proxy logs the presented secret).

### The baked RUST_LOG (why one line holds in production)

cja's `prepare_routes` attaches a TraceLayer whose default filter
(`info,engine_proxy=trace,tower_http=debug`) prints 2-3 lines per request
plus an ERROR on every passed-through snake 5xx. The image bakes:

```
ENV RUST_LOG=info,cja::server::trace=warn,tower_http=off,tower_http::catch_panic=error
```

so the `engine_proxy` request line is the only per-request stdout record
(~1 line/request, ~2M/day/region) — except CatchPanicLayer's ERROR, kept by
longest-prefix (it is the panic evidence). **Caveat:** docker `--env-file`/`-e`
beats image `ENV` — never add `RUST_LOG` to `/run/engine-proxy.env`; it would
silently restore the noisy default. The CI smoke asserts the shape (exactly
one `proxied request` line per probe, zero `tower_http`/`cja::server::trace`
lines) and proves the check discriminates with a `-e RUST_LOG=info` mutation
run that must fail it.

## Metrics (`/metrics`, token-gated)

`engine_proxy_requests_total{outcome}` (all outcomes, including the drop-guard
ones), `engine_proxy_snake_status_total{class}`, `engine_proxy_request_duration_ms`
(histogram), `engine_proxy_tls_handshake_ms` (histogram),
`engine_proxy_connections_total` (every connector dial — can exceed the
fresh-labelled-request count, see `fresh_connection` above),
`engine_proxy_body_aborts_total{reason}`, `engine_proxy_process_start_time_seconds`.

## Shutdown

SIGTERM ⇒ cja's `Supervisor` cancels its token ⇒ `run_server_until`'s graceful
shutdown stops accepting and drains in-flight requests; stragglers are
aborted at 4s (drain 1s + grace 3s, both baked) — under the 10s
`docker stop` SIGKILL default in the infrastructure module. Requests whose
budget exceeds 4s are aborted and logged `shutdown_aborted`. The
`drain-check.sh` script asserts a slow in-flight request is drained or
deadline-aborted (and logged), not just that the process exits.

## Sizing / build

- Builder `rust:1.99-bookworm`, runtime `gcr.io/distroless/cc-debian12`
  (glibc 2.36 both — never drop `-bookworm`), non-root `nonroot:nonroot`,
  no shell, `webpki-roots` for TLS. Stripped binary ~13 MB (amd64); image
  size recorded by the CI `image` job.
- cja links `sqlx`/`tower-cookies` unconditionally (feature-gating upstream
  is a recorded follow-up); the proxy itself constructs no pool — DB-less at
  runtime, proven by the CI smoke which sets no `DATABASE_URL`.
- `prometheus = { default-features = false }` (text encoder only; defaults
  pull protobuf). No cargo-deny/audit gate exists in this repo to keep green;
  the lock diff from adding the crate is `prometheus` + `protobuf`-free deps
  only.

## Differences vs the Go proxy (decision log)

| behavior | Rust | Go | decision |
|---|---|---|---|
| gzip passthrough | no `Accept-Encoding` added, no decompression | transport may transparently decompress when caller sent no A-E | accepted (arena's direct path is identical) |
| request body cap | 413 past 1 MiB | none | accepted (budget still bounds the read) |
| declared oversized response | 502 + `5:` before forwarding headers | streams until the reader gives up | accepted (classifiable fault, not truncated 200) |
| request body | buffered ≤1 MiB under the budget | streamed copy under ctx deadline | accepted (snake payloads are small JSON) |
| auth-failure log | "bad or missing proxy auth" | logs the presented secret | accepted (token hygiene) |
| `token` scheme matching | case-sensitive | case-sensitive | parity |
| `client_headers`/`client_body` | body-wrapper approximation of httptrace | httptrace callbacks | accepted (near-zero for buffered JSON) |
| metrics series | prometheus text at `/metrics` | none | ours |
| `HEALTH_CHECK_URLS` | env | `--test-server` flag | identical runtime behavior (bootstrap passes none) |
| panic in handler | 500 + `0:` (CatchPanicLayer), panic logged at `tower_http::catch_panic`; **no latency header** — the hook has no request context | connection closed | accepted (better signal; arena classifies both as a proxy fault → direct retry, DEV-1946) |
| per-request INFO line | yes (this doc) | errors only | ours — the point of the rewrite |
| connect strategy | staggered race, no per-attempt cap, 250ms stagger | DualStack race, 300ms fallback, 30s dialer | same spirit, different mechanics |
| body-phase deadline abort | connection killed; arena sees a proxy transport fault and (DEV-1946) retries `/move` directly | same (ctx deadline) | match, not a diff |
| self-signed CA leaf cert | rejected (`CaUsedAsEndEntity` — webpki refuses CA certs as end entities) | accepted (Go's x509 tolerates it) | accepted — snakes needing `ENGINE_PROXY_EXTRA_CA` must present a proper CA+leaf chain, verified in the load check |
| empty-body success labeling | `outcome=success` via eager finish (hyper never polls a CL:0 body) | n/a | fixed during the load check (was mislabeled `client_cancelled`) |
| `getAlive` failing probes | answers as soon as every check has failed (does not wait out the 3s window) | no-op `break` in select loop | don't copy Go's bug |

The Go differential is a **local** check — `engine-proxy/scripts/go-contract.sh`
builds the Go image from a private local checkout at the pinned commit and
re-runs the full contract suite against it (`ENGINE_PROXY_GO_TARGET=…`,
`--network host`), printing the observation table for any row that drifts.
Last local run (2026-10-26, Go @ 808c248): all cases green; the single
observed diff is the declared-oversized-response row above (documented,
accepted). Not a CI job: the Go source repo is private, has no public image
(Docker Hub `conflictserpent` has zero public repos), and CI may not gain
secrets.

## Deploy / canary notes (see also memory: projects/arena/docs/engine-regions.md)

- Target: the EC2 + Caddy module from infrastructure#51 (`aws/modules/engine-proxy`),
  **us-east-1 = Rust canary, eu-central-1 stays Go** as the 48h control.
  Flip the module's `image` var for us-east-1 to the new ECR repo (image push
  is out-of-band; CI does not push). No Cloud Run deploy is involved.
- **The restart window**: `ExecStartPre` (SSM fetch + ECR login + `docker pull`)
  plus `docker rm -f` before the new container listens leaves Caddy with no
  upstream for seconds — deploys/token rotations cost that window and must be
  marked/excluded from the 48h comparison.
- Arena-side switch: point `ENGINE_PROXY_US_EAST4_URL` at the AWS endpoint
  with `ENGINE_PROXY_AUTH_TOKEN` equal to the proxy's
  `BATTLESNAKE_PROXY_AUTH_TOKEN` (compare by `sha256sum`, never by printing).
  **Do not add `RUST_LOG` to `/run/engine-proxy.env`** — it would override
  the baked filter (see above).
- Eyes envs (`EYES_ORG_ID`/`EYES_APP_ID`/`EYES_URL`) go in the module deploy
  notes and the canary checklist; at smoke time assert the boot log does
  **not** contain `Skipping Eyes layer` (a stale Eyes config went dark for
  weeks on arena once).
- 48h comparison: local-guard faults per 100k calls + proxy timeouts, judged
  **per snake and hourly** (one deadline-hugging snake can fake a regional
  regression), deploy/restart windows marked and excluded. Count by snakes
  via `snake_turns` ⋈ `game_battlesnakes`, not by distinct warn-log values.
  Watch the duel-frame proxy-fault signature (`StatusCode 0, Latency "0",
  Error ""` mid-game = proxy fault → fallback) and the community-triage
  equivalence ("moved out of bounds with 0ms latency" / "never received the
  /move request" = proxy-side, not the snake).
- Whether the 502/504 storm → instance-ejection loop seen on Cloud Run
  (DEV-1614) still applies on EC2 is unresolved — and mooted by the move; the
  per-request log line exists precisely to settle it if it recurs.

## Local checks

- `engine-proxy/scripts/load-check.sh` — symmetric Rust-vs-Go load check
  (50 mock snakes, half TLS behind a generated mini-CA + leaf, 200
  concurrency, 60s per phase): p50/p99 added latency, mock-side accept
  counts (new-connection share), RSS.
- `engine-proxy/scripts/drain-check.sh` — SIGTERM drain discrimination check.
- `engine-proxy/scripts/go-contract.sh` — local Go differential (builds the
  Go image from a local private checkout at the pinned commit; not CI — see
  above).
- Contract suite: `SQLX_OFFLINE=true cargo test -p arena --bin arena
  snake_client_contract_tests` (Rust in-process by default; Go with
  `ENGINE_PROXY_GO_TARGET=docker://<image>` — `--network host` is required so
  the container can reach the suite's loopback mock snakes; the harness
  labels + reaps its containers).

### Load-check results (local, 2026-10-26, Go 808c248 vs this PR)

Added latency = request wall time minus the mock's own delay, 200 concurrency,
60s per drive, run order Go → Rust → Rust-again. New-connection share is the
mock-side TCP-accept delta over requests (authoritative for both proxies).

| phase | proxy | p50 added | p99 added | new-conn share | RSS |
|---|---|---|---|---|---|
| http | Go | 1.6 ms | 3.8 ms | **34.5%** | 35.8 MiB |
| http | Rust | 1.5 ms | 2.7 ms | 10.1% | 34.0 MiB |
| http | Rust (warm) | 1.6 ms | 3.7 ms | 10.6% | — |
| https | Go | 1.9 ms | 3.9 ms | **37.2%** | 37.6 MiB |
| https | Rust | 1.6 ms | 3.3 ms | 11.4% | 34.7 MiB |
| https | Rust (warm) | 1.6 ms | 6.1 ms | 11.1% | — |

Zero failures across all six drives (~74k requests each). The Go column
reproduces DEV-1499 item 2 locally — ~35% of steady-state requests land on a
brand-new connection (`MaxIdleConnsPerHost: 2`), paying a full TCP+TLS
handshake on the critical path in the https phase; Rust's 8-idle-per-host pool
cuts that to ~11% (the residual is mock-side idle-timeout churn) with
equal-or-better p50/p99 and ~3 MiB less RSS.
