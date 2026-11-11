#!/usr/bin/env bash
# Builds wowsir, serves the fixtures, runs every .wowsir test. Exit code != 0 on failure.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
K="$PWD/target/release/wowsir"
PORT=8765
# own home = own daemon (always this build) and no saved logins from the user's machine
export WOWSIR_HOME="$(mktemp -d "${TMPDIR:-/tmp}/wowsir-smoke.XXXXXX")"
export WOWSIR_AUTH_DIR="$WOWSIR_HOME/auth"
PY=$(command -v python3 || command -v python)   # Windows runners only have `python`
"$PY" tests/serve.py $PORT tests/fixtures & SRV=$!
trap 'kill $SRV 2>/dev/null; "$K" stop >/dev/null 2>&1; rm -rf "$WOWSIR_HOME"' EXIT
# Python starts slowly on Windows runners: wait (both names the tests use), and fail loudly
ready=""
for _ in $(seq 300); do
  if curl -s -o /dev/null "http://localhost:$PORT/index.html" && curl -s -o /dev/null "http://127.0.0.1:$PORT/index.html"; then ready=1; break; fi
  sleep 0.1
done
[ -n "$ready" ] || { echo "fixture server did not start on port $PORT"; exit 1; }
cd tests
# each file in its own fresh session, all at once
"$K" -s smoke -q run *.wowsir --parallel 8
