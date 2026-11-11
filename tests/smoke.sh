#!/usr/bin/env bash
# Builds kite, serves the fixtures, runs every .kite test. Exit code != 0 on failure.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
K=./target/release/kite
PORT=8765
(cd tests/fixtures && python3 -m http.server $PORT >/dev/null 2>&1) & SRV=$!
trap 'kill $SRV 2>/dev/null' EXIT
for _ in $(seq 50); do curl -s -o /dev/null localhost:$PORT && break; sleep 0.1; done
cd tests
# each file in its own fresh session, all at once
../target/release/kite -s smoke -q run *.kite --parallel 8
