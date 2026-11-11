#!/usr/bin/env bash
# Builds wowsir, serves the fixtures, runs every .wowsir test. Exit code != 0 on failure.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
K=./target/release/wowsir
PORT=8765
(cd tests/fixtures && python3 -m http.server $PORT >/dev/null 2>&1) & SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
for _ in $(seq 50); do curl -s -o /dev/null localhost:$PORT && break; sleep 0.1; done
cd tests
# each file in its own fresh session, all at once
../target/release/wowsir -s smoke -q run *.wowsir --parallel 8
