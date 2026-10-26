#!/usr/bin/env bash
# Manual SIGTERM-drain check for DEV-1943 (plan step 7): a discriminating
# test, not a vibes check.
#
# (a) start the real binary with a mock snake that stalls mid-body under a
#     multi-second budget — a request guaranteed still in flight at SIGTERM;
# (b) SIGTERM the process, time the exit with a kill -0 loop at 0.1s
#     intervals — it must exit inside the shutdown budget (4s < docker's 10s);
# (c) assert the in-flight request produced its terminal log line
#     (shutdown_aborted or a Timeout), not just that the process exited;
# (d) assert the port is free before any relaunch.
#
# Mutation run: `DRAIN_MUTATION=1` rebuilds main.rs with the server spawn
# removed — the process then never registers the graceful-shutdown drain and
# the exit-time assertion must FAIL, proving the check discriminates.
set -euo pipefail

cd "$(dirname "$0")/../.."
BUILD_DIR="${CARGO_BUILD_BUILD_DIR:-target}"
OUT=./drain-check
mkdir -p "$OUT"
TOKEN="drain-check-token-$$"
PORT=18110
SNAKE_PORT=18111

python3 - "$SNAKE_PORT" &>/dev/null <<'EOF' &
import socket, sys, time
port = int(sys.argv[1])
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", port)); srv.listen(8)
while True:
    conn, _ = srv.accept()
    conn.settimeout(5)
    try:
        conn.recv(65536)
        conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{\"move\":")
    except OSError:
        pass
    # hold the connection open, never finish the body
    try:
        time.sleep(300)
    except Exception:
        pass
EOF
SNAKE_PID=$!

# cargo may place the binary under release/deps/ with a fingerprint suffix
# (shared build dirs skip the top-level hardlink) — resolve either location.
BIN="$BUILD_DIR/release/engine"
if [ ! -x "$BIN" ]; then
  BIN=$(find "$BUILD_DIR/release/deps" -maxdepth 1 -type f -perm -u=x \
    -name 'engine-*' ! -name '*.d' ! -name '*.so' | sort | tail -1)
fi
[ -x "$BIN" ] || { echo "engine binary not found under $BUILD_DIR/release" >&2; exit 1; }

# Mutation-mode backup handling: the backup lives at a mktemp path (never a
# fixed one another run could clobber), and the restore runs from a trap on
# EXIT — and on INT/TERM via `exit`, which re-enters the EXIT trap — so an
# interrupted run can never leave the mutated main.rs in the tree.
MUTATION_BACKUP=""
do_restore_main_rs() {
  if [ -n "$MUTATION_BACKUP" ] && [ -f "$MUTATION_BACKUP" ]; then
    cp "$MUTATION_BACKUP" engine-proxy/src/main.rs
    rm -f "$MUTATION_BACKUP"
    MUTATION_BACKUP=""
  fi
}
restore_main_rs_on_exit() {
  local status=$?
  do_restore_main_rs
  exit "$status"
}
trap restore_main_rs_on_exit EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [ "${DRAIN_MUTATION:-0}" = 1 ]; then
  echo "== mutation: building WITHOUT the server spawn"
  _backup=$(mktemp /tmp/engine-proxy-main.XXXXXX)
  cp engine-proxy/src/main.rs "$_backup"
  MUTATION_BACKUP="$_backup" # armed only once the backup copy is complete
  sed -i 's/^    supervisor.spawn($/    let _ignore_supervisor = \&mut supervisor; if false { supervisor.spawn(/;' engine-proxy/src/main.rs
  # close the extra brace on the spawn call
  python3 - <<'PYEOF'
p = 'engine-proxy/src/main.rs'
s = open(p).read()
s = s.replace('''        cja::server::run_server_until(router, shutdown.cancelled_owned()),
    );''', '''        cja::server::run_server_until(router, shutdown.cancelled_owned()),
    ); }''')
open(p, 'w').write(s)
PYEOF
  # on failure the EXIT trap restores main.rs
  cargo build --release -p engine-proxy >"$OUT/mutation-build.log" 2>&1 || { echo "mutation build failed" >&2; exit 1; }
  do_restore_main_rs # eager restore too: the mutated tree lives for seconds, not minutes
else
  cargo build --release -p engine-proxy >"$OUT/build.log" 2>&1
fi

PORT=$PORT BATTLESNAKE_PROXY_AUTH_TOKEN="$TOKEN" "$BIN" proxy_server >"$OUT/proxy.log" 2>&1 &
PROXY_PID=$!

for _ in $(seq 1 50); do
  curl -fsS "http://127.0.0.1:$PORT/healthz/alive" >/dev/null 2>&1 && break
  sleep 0.2
done

echo "== firing an in-flight request (5s budget, snake stalls mid-body)"
curl -s -o "$OUT/inflight-body.txt" -m 20 -X POST "http://127.0.0.1:$PORT/" \
  -H "X-Request-URI: http://127.0.0.1:$SNAKE_PORT/move" \
  -H "X-Battlesnake-Timeout-Ms: 5000" \
  -H "X-Proxy-Authorization: token $TOKEN" &
CURL_PID=$!
sleep 1 # request is provably in flight (headers sent by the snake at ~0ms)

echo "== SIGTERM"
kill -TERM $PROXY_PID
START=$(date +%s.%N)
while kill -0 $PROXY_PID 2>/dev/null; do sleep 0.1; done
END=$(date +%s.%N)
EXIT_SECS=$(python3 -c "print(f'{$END - $START:.2f}')")
kill $CURL_PID 2>/dev/null || true
kill $SNAKE_PID 2>/dev/null || true

echo "== exit took ${EXIT_SECS}s"
echo "== terminal log lines for the in-flight request:"
grep -E 'outcome=' "$OUT/proxy.log" | grep ":$SNAKE_PORT" || true

# (c) the in-flight request was drained AND logged, not just dropped
if ! grep -qE "outcome=(shutdown_aborted|timeout)" "$OUT/proxy.log"; then
  echo "FAIL: no terminal log line for the drained request" >&2
  exit 1
fi
# (b) exit inside the 4s budget (1s job drain + 3s exit grace)
if python3 -c "import sys; sys.exit(0 if $EXIT_SECS < 5.0 else 1)"; then
  echo "PASS: drained and exited in ${EXIT_SECS}s"
else
  echo "FAIL: exit took ${EXIT_SECS}s (budget 4s + slack)" >&2
  exit 1
fi
# (d) port free before relaunch
if curl -s -o /dev/null --max-time 1 "http://127.0.0.1:$PORT/healthz/alive"; then
  echo "FAIL: port $PORT still answering after exit" >&2
  exit 1
fi
echo "PASS: port is free"
