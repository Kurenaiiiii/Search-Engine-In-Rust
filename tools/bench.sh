#!/bin/bash
# Frozen benchmark driver. Run from repo root: bash tools/bench.sh <corpus.ndjson>
# Steps: index with se-cli (throughput), query latency via `se-cli bench`,
# SQLite FTS5 comparison, SVG charts. Results go to bench/results/.
set -euo pipefail
CORPUS="${1:?usage: bash tools/bench.sh <corpus.ndjson>}"
QUERIES="tools/bench_queries.txt"
OUT="bench/results"
mkdir -p "$OUT"
cargo build --release -p se-cli 2>&1 | tail -1
rm -rf "$OUT/se_data"
echo "== indexing =="
./target/release/se-cli --dir "$OUT/se_data" index "$CORPUS" | tee "$OUT/indexing.txt"
./target/release/se-cli --dir "$OUT/se_data" stats | tee "$OUT/stats.txt"
echo "== latency =="
./target/release/se-cli --dir "$OUT/se_data" bench --queries "$QUERIES" --iters 20 | tee "$OUT/latency.json"
echo "== sqlite fts5 =="
python3 tools/compare_sqlite.py "$CORPUS" "$QUERIES" | tee "$OUT/sqlite.json"
echo "== charts =="
python3 tools/charts.py "$OUT/latency.json" "$OUT"
echo "results in $OUT"
