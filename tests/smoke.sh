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

# long runs of similar elements fold to a few plus a count; --in lists them all
"$K" -s markers -q open localhost:$PORT/markers.html >/dev/null
out=$("$K" -s markers snap)
echo "$out" | grep -q '… 55 more button' || fail "snap did not fold the markers: $out"
echo "$out" | grep -q 'button "Refresh"' || fail "content after the markers is missing: $out"
[ "$("$K" -s markers snap --in .map | grep -c 'button "Station')" = 60 ] || fail "snap --in did not list every marker"
out=$("$K" -s markers click button:Refresh)
echo "$out" | grep -q '… 55 more button' || fail "redrawn markers did not fold: $out"
[ "$(echo "$out" | wc -l)" -lt 20 ] || fail "output after redrawing the markers too long: $out"
echo "✓ similar siblings fold in snapshots and diffs"

# a page the browser kept: say so on open, and fetch it fresh on demand
"$K" -s cache -q open localhost:$PORT/cached.html >/dev/null
out=$("$K" -s cache open localhost:$PORT/cached.html)
echo "$out" | grep -q "page came from the browser cache.*max-age=600" || fail "no cache warning on a cached page: $out"
"$K" -s cache cache | grep -q "document 200 from the browser cache" || fail "cache did not report the cached document"
out=$("$K" -s cache reload --hard)
if echo "$out" | grep -q "came from the browser cache"; then fail "reload --hard used the cache: $out"; fi
"$K" -s cache cache | grep -q "document 200 from the server" || fail "reload --hard did not ask the server"
echo "✓ a page served from the browser cache is reported; reload --hard fetches it fresh"

# ambiguous targets: a warning that names the twins, a failure inside a test file
"$K" -s twins -q open localhost:$PORT/twins.html >/dev/null
out=$("$K" -s twins click button:Delete) || fail "ambiguous click failed outside a test: $out"
echo "$out" | grep -q "2 elements match button:Delete: used @e[0-9]* (in \"Buy milk\"); also @e[0-9]* (in \"Walk dog\") → 'text=Walk dog >> button:Delete'" || fail "no ambiguity warning: $out"
"$K" -s twins record show | grep -q "click 'text=Buy milk >> button:Delete'" || fail "ambiguous click not recorded as the element it hit"
printf 'open localhost:%s/twins.html\nclick button:Delete\n' $PORT > "$WOWSIR_HOME/ambiguous.wowsir"
if "$K" -s amb run "$WOWSIR_HOME/ambiguous.wowsir" >/dev/null; then fail "a test with an ambiguous target passed"; fi
echo "✓ ambiguous targets warn, and fail inside a test file"

# Playwright: a ref's stable target and locator, and a test file converted
loc=$("$K" -s twins locator 'text=Walk dog >> button:Delete')
echo "$loc" | grep -q "^wowsir      'text=Walk dog >> button:Delete'" || fail "locator: $loc"
echo "$loc" | grep -q "^playwright  page.locator('\*', { has: page.getByText('Walk dog') })" || fail "locator: $loc"
spec=$("$K" export twins.wowsir -o -) || fail "export twins.wowsir"
echo "$spec" | grep -qF "await near(page.getByText('Walk dog'), page.getByRole('button', { name: 'Delete', exact: true })).click();" || fail "export: $spec"
"$K" -s twins record save "$WOWSIR_HOME/rec.spec.ts" | grep -q "Playwright test" || fail "record save .spec.ts"
grep -q "import { test, expect" "$WOWSIR_HOME/rec.spec.ts" || fail "record save .spec.ts wrote no Playwright test"
echo "✓ locator, export and record save .spec.ts produce Playwright code"
