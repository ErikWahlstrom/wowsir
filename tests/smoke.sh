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
export WOWSIR_READONLY_FILE="$WOWSIR_HOME/readonly"   # not the user's read-only site list
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

# read-only guarantees that need a failing step (a .wowsir file stops at the first failure)
fail() { echo "FAIL: $*"; exit 1; }
"$K" -s ro -q open localhost:$PORT/agent.html >/dev/null
"$K" -s ro readonly strict >/dev/null
if "$K" -s ro -q click button:Save >/dev/null; then fail "strict read-only let a write pass"; fi
echo "✓ strict read-only fails the step that writes"
echo "ro.localhost lock" > "$WOWSIR_READONLY_FILE"   # *.localhost is loopback in Chrome
out=$("$K" -s lock open ro.localhost:$PORT/agent.html) || fail "open ro.localhost"
echo "$out" | grep -q "listed in .*(locked)" || fail "site rule not applied: $out"
if "$K" -s lock readonly off >/dev/null; then fail "a locked site's read-only could be turned off"; fi
"$K" -s lock -q click button:Save >/dev/null
"$K" -s lock text '#status' | grep -q "save failed" || fail "write went through on a locked site"
echo "✓ listed site opens read-only and stays locked"

# outline checks (a .wowsir file can't assert on `snap` output)
"$K" -s outline -q open localhost:$PORT/modal.html >/dev/null
"$K" -s outline -q click button:Ask >/dev/null
snap=$("$K" -s outline snap -i)
echo "$snap" | grep -q 'dialog "Unknown Tool Requested" modal' || fail "CSS-shown <dialog> missing from the outline: $snap"
echo "$snap" | grep -q 'button "Submit"' || fail "controls of a CSS-shown <dialog> missing: $snap"
if echo "$snap" | grep -qE 'Native hidden|Faded hidden'; then fail "a hidden <dialog> is in the outline: $snap"; fi
echo "✓ a <dialog> shown by CSS is in the outline, hidden ones are not"
