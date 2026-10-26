#!/usr/bin/env bash
# Local Go contract differential (DEV-1943, PR #277 review round 1).
#
# Why local and not CI: the Go engine source repo (BattlesnakeOfficial/engine)
# is private, has no LICENSE file, and has no public image (Docker Hub
# conflictserpent org has zero public repos — verified 2026-10-26), and CI may
# not gain secrets. Coordinator decision 2026-10-26: the differential is a
# local check; the recorded observations live in docs/engine-proxy.md.
#
# Builds the Go image from a local engine checkout at a pinned commit and
# re-runs the arena contract suite against it via ENGINE_PROXY_GO_TARGET.
# The suite runs the Go container with --network host (required so it can
# reach the suite's host-loopback mock snakes) and cleans up its container
# (--rm + label + watchdog; see snake_client_contract_tests.rs).
#
# Usage:
#   engine-proxy/scripts/go-contract.sh            # default pin 808c248
#   ENGINE_SHA=<sha> engine-proxy/scripts/go-contract.sh
#   ENGINE_SOURCE_DIR=<dir> ...                    # default ~/code/BattlesnakeOfficial/engine
#
# Env:
#   ENGINE_SOURCE_DIR  local engine checkout (must contain the pinned SHA)
#   ENGINE_SHA         commit to build (default 808c248, the plan's pin)
#   ENGINE_GO_IMAGE    image tag (default battlesnake-engine-go:contract)
#   LOG_DIR            output dir for the log file (default /tmp/mull-scratch)

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
ENGINE_DIR="${ENGINE_SOURCE_DIR:-$HOME/code/BattlesnakeOfficial/engine}"
ENGINE_SHA="${ENGINE_SHA:-808c248}"
IMAGE="${ENGINE_GO_IMAGE:-battlesnake-engine-go:contract}"
LOG_DIR="${LOG_DIR:-/tmp/mull-scratch}"
LOG="$LOG_DIR/go-contract-$(date +%Y%m%d-%H%M%S).log"

mkdir -p "$LOG_DIR"

if [ ! -d "$ENGINE_DIR/.git" ]; then
  echo "Go engine checkout not found at $ENGINE_DIR" >&2
  echo "Clone it first (private repo, needs your credentials), or set ENGINE_SOURCE_DIR." >&2
  exit 1
fi
if ! git -C "$ENGINE_DIR" cat-file -e "$ENGINE_SHA^{commit}" 2>/dev/null; then
  echo "Pinned commit $ENGINE_SHA not present in $ENGINE_DIR (fetch it first)." >&2
  exit 1
fi

echo "== Building Go engine image at $ENGINE_SHA (git archive — no checkout mutation)"
# git archive streams the tree at the pin; no working-tree changes, no leftover worktree.
IMAGE="$IMAGE" ENGINE_DIR="$ENGINE_DIR" ENGINE_SHA="$ENGINE_SHA" LOG="$LOG" bash -c '
  set -euo pipefail
  tmp="$(mktemp -d)"
  trap "rm -rf \"$tmp\"" EXIT
  git -C "$ENGINE_DIR" archive "$ENGINE_SHA" | tar -x -C "$tmp"
  docker build -q -t "$IMAGE" "$tmp" >>"$LOG" 2>&1
'

echo "== Running the arena contract suite against $IMAGE (log: $LOG)"
# timeout -k so a hang fails instead of wedging; output to a file (never | tail).
cd "$ROOT"
if ! SQLX_OFFLINE=true \
  ENGINE_PROXY_GO_TARGET="docker://$IMAGE" \
  timeout -k 10 900 cargo test -p arena --bin arena snake_client_contract_tests -- --nocapture \
  >"$LOG" 2>&1; then
  echo "== FAILED (tail of $LOG):"
  tail -40 "$LOG"
  exit 1
fi

echo "== Green. Observation table (contract_diff_table) from $LOG:"
grep -A40 '^| case |' "$LOG" || echo "(no table rows printed)"

# Belt and braces: the suite watches its own container, but assert nothing
# labeled is left running.
left="$(docker ps -q --filter 'label=dev1943.contract=go')"
if [ -n "$left" ]; then
  echo "leftover contract containers: $left — removing" >&2
  docker rm -f $left >/dev/null
fi
echo "Done: $LOG"
