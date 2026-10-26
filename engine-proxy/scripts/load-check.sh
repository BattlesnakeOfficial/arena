#!/usr/bin/env bash
# Local load check for DEV-1943: Rust engine proxy vs Go engine proxy under
# identical mock-snake load, both running as --network host containers
# (identical network path, no docker-proxy/NAT asymmetry).
#
# Phases: (1) http, (2) https — the TLS phase is the DEV-1499
# handshake-storm scenario. Run order: Go, Rust, Rust-again (host-warmup
# control). The Rust proxy must not be worse; if it is, stop and investigate.
#
# Results land in ./load-results/ (captured to files, never piped through
# tail). Requires: docker, openssl, python3, and a cargo build dir containing
# the mock_snake and load_check example binaries.
#
# Usage:
#   cargo build --release -p engine-proxy --examples
#   CARGO_BUILD_BUILD_DIR=/tmp/mull-scratch/arena-1943 engine-proxy/scripts/load-check.sh
set -euo pipefail

cd "$(dirname "$0")/../.."
OUT=./load-results
BUILD_DIR="${CARGO_BUILD_BUILD_DIR:-target}"
# Example binaries can carry cargo's fingerprint suffix (mock_snake-<hash>)
# in shared build dirs; resolve by prefix, skipping .d files.
resolve_example() {
  local f
  f=$(find "$BUILD_DIR/release/examples" -maxdepth 1 -type f -perm -u+x \
    -name "$1-*" ! -name '*.d' | sort | head -1)
  [ -n "$f" ] || f="$BUILD_DIR/release/examples/$1"
  echo "$f"
}
MOCK_SNAKE=$(resolve_example mock_snake)
LOAD_CHECK=$(resolve_example load_check)
DURATION="${DURATION:-60s}"
CONCURRENCY="${CONCURRENCY:-200}"
PORT_BASE=19100
STATS_BASE=29001
SNAKES=50
GO_PROXY_PORT=18100
RUST_PROXY_PORT=18101
LOAD_TOKEN="load-check-$$-$RANDOM"

PIDS=()

cleanup() {
  for pid in "${PIDS[@]:-}"; do
    kill "$pid" 2>/dev/null || true
  done
  docker rm -f ep-load-go ep-load-rust >/dev/null 2>&1 || true
}
trap cleanup EXIT

for f in "$MOCK_SNAKE" "$LOAD_CHECK"; do
  if [ ! -x "$f" ]; then
    echo "missing $f — run: cargo build --release -p engine-proxy --examples" >&2
    exit 1
  fi
done

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

wait_alive() {
  for _ in $(seq 1 60); do
    curl -fsS "http://127.0.0.1:$1/healthz/alive" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  echo "proxy on :$1 never became alive" >&2
  return 1
}

rss() {
  docker stats --no-stream --format '{{.Name}} {{.MemUsage}}' "$1" | head -1
}

# ---------------------------------------------------------------------------
mkdir -p "$OUT"
echo "== building images"
docker build -f engine-proxy/Dockerfile -t engine-proxy-load . >"$OUT/build-rust.log" 2>&1
GO_REPO="$HOME/code/BattlesnakeOfficial/engine"
if [ ! -d "$GO_REPO" ]; then
  echo "Go engine repo not found at $GO_REPO" >&2
  exit 1
fi
(
  cd "$GO_REPO"
  git checkout -q 808c248
  docker build -q -t battlesnake-engine-go . >"$OLDPWD/$OUT/build-go.log" 2>&1
)

echo "== TLS material for the https phase: a mini CA + a server leaf"
# A proper CA+leaf pair, NOT a self-signed server cert: openssl req -x509
# marks its output CA:TRUE, and rustls/webpki refuses a CA cert presented
# as the end entity (CaUsedAsEndEntity) — Go tolerates it, we must not rely
# on that. The CA goes into the proxies' trust stores; the leaf serves TLS.
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$OUT/ca-key.pem" -out "$OUT/ca.pem" -days 2 \
  -subj "/CN=mock-snake-ca" \
  -addext "basicConstraints=critical,CA:TRUE" 2>"$OUT/openssl.log"
openssl req -newkey rsa:2048 -nodes -keyout "$OUT/key.pem" -out "$OUT/csr.pem" -days 2 \
  -subj "/CN=mock-snake" 2>>"$OUT/openssl.log"
openssl x509 -req -in "$OUT/csr.pem" -CA "$OUT/ca.pem" -CAkey "$OUT/ca-key.pem" -CAcreateserial \
  -out "$OUT/cert.pem" -days 2 \
  -extfile <(printf 'subjectAltName=IP:127.0.0.1,DNS:localhost\nbasicConstraints=critical,CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n') 2>>"$OUT/openssl.log"

echo "== starting $SNAKES mock snakes (0-24 http, 25-49 https)"
for i in $(seq 0 $((SNAKES - 1))); do
  port=$((PORT_BASE + i))
  stats=$((STATS_BASE + i))
  args=(--port "$port" --stats-port "$stats" --delay-min-ms 20 --delay-max-ms 300)
  if [ "$i" -ge 25 ]; then
    args+=(--tls "$PWD/$OUT/cert.pem" "$PWD/$OUT/key.pem")
  fi
  "$MOCK_SNAKE" "${args[@]}" >"$OUT/snake-$port.log" 2>&1 &
  PIDS+=($!)
done
for i in $(seq 0 $((SNAKES - 1))); do
  stats=$((STATS_BASE + i))
  for _ in $(seq 1 50); do
    curl -fsS "http://127.0.0.1:$stats/stats" >/dev/null 2>&1 && break
    sleep 0.2
  done
done
echo "snakes up"

drive() { # $1=proxy_base_url $2=phase(http|https) $3=label
  local lo hi
  if [ "$2" = http ]; then lo=0; hi=24; else lo=25; hi=49; fi
  local https_flag=""
  [ "$2" = https ] && https_flag="--https"
  "$LOAD_CHECK" --proxy "$1" --token "$LOAD_TOKEN" \
    --snake-ports "$((PORT_BASE + lo))-$((PORT_BASE + hi))" \
    --stats-ports "$((STATS_BASE + lo))-$((STATS_BASE + hi))" \
    --concurrency "$CONCURRENCY" --duration "$DURATION" $https_flag |
    tee "$OUT/$3-$2.txt"
}

start_go() { # $1=extra docker args...
  docker rm -f ep-load-go >/dev/null 2>&1 || true
  docker run -d --name ep-load-go --network host \
    -e PORT=$GO_PROXY_PORT -e BATTLESNAKE_PROXY_AUTH_TOKEN="$LOAD_TOKEN" \
    "$@" \
    --entrypoint engine battlesnake-engine-go proxy_server >/dev/null
  wait_alive $GO_PROXY_PORT
}

start_rust() { # $1=extra docker args...
  docker rm -f ep-load-rust >/dev/null 2>&1 || true
  docker run -d --name ep-load-rust --network host \
    -e PORT=$RUST_PROXY_PORT -e BATTLESNAKE_PROXY_AUTH_TOKEN="$LOAD_TOKEN" \
    "$@" \
    --entrypoint engine engine-proxy-load proxy_server >/dev/null
  wait_alive $RUST_PROXY_PORT
}

echo "== phase 1: http"
start_go
drive "http://127.0.0.1:$GO_PROXY_PORT" http go
rss ep-load-go | tee "$OUT/go-http-rss.txt"

start_rust
drive "http://127.0.0.1:$RUST_PROXY_PORT" http rust
rss ep-load-rust | tee "$OUT/rust-http-rss.txt"
drive "http://127.0.0.1:$RUST_PROXY_PORT" http rust2

echo "== phase 2: https (TLS handshake scenario)"
start_go -e SSL_CERT_FILE=/certs/ca.pem -v "$PWD/$OUT/ca.pem:/certs/ca.pem:ro"
drive "http://127.0.0.1:$GO_PROXY_PORT" https go
rss ep-load-go | tee "$OUT/go-https-rss.txt"

start_rust -e ENGINE_PROXY_EXTRA_CA=/certs/ca.pem -v "$PWD/$OUT/ca.pem:/certs/ca.pem:ro"
drive "http://127.0.0.1:$RUST_PROXY_PORT" https rust
rss ep-load-rust | tee "$OUT/rust-https-rss.txt"
drive "http://127.0.0.1:$RUST_PROXY_PORT" https rust2

echo "== done; results in $OUT"
