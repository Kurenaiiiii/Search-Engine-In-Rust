#!/bin/bash
# Helper for the v0.2.0 optimization round: runs one full benchmark trial
# into OUT dir. NOT part of the frozen harness (tools/bench.sh,
# tools/bench_queries.txt, tools/compare_sqlite.py, tools/eval_beir.py,
# tools/make_corpus.py, tools/charts.py are untouched); it only invokes the
# same frozen commands plus the manual merged-index steps used last round.
set -u
OUT="${1:?usage: run_trials.sh <out-dir>}"
CORPUS=bench/corpus.ndjson
QUERIES=tools/bench_queries.txt
mkdir -p "$OUT"
snapshot() { { echo "--- $1 $(date -u +%FT%TZ)"; cat /proc/loadavg; uptime; free -m | head -2; nproc; } > "$OUT/load_$1.txt"; }
snapshot before
rm -rf "$OUT/se_data"
./target/release/se-cli --dir "$OUT/se_data" index "$CORPUS" | tee "$OUT/indexing.txt"
./target/release/se-cli --dir "$OUT/se_data" stats | tee "$OUT/stats.txt"
./target/release/se-cli --dir "$OUT/se_data" bench --queries "$QUERIES" --iters 20 | tee "$OUT/latency_multi.json"
./target/release/se-cli --dir "$OUT/se_data" bench --queries "$QUERIES" --iters 20 --threads 1 | tee "$OUT/latency_single.json"
./target/release/se-cli --dir "$OUT/se_data" optimize | tee "$OUT/optimize.txt"
./target/release/se-cli --dir "$OUT/se_data" stats | tee -a "$OUT/stats.txt"
./target/release/se-cli --dir "$OUT/se_data" bench --queries "$QUERIES" --iters 20 | tee "$OUT/latency_merged_mt.json"
./target/release/se-cli --dir "$OUT/se_data" bench --queries "$QUERIES" --iters 20 --threads 1 | tee "$OUT/latency_merged_st.json"
python3 tools/compare_sqlite.py "$CORPUS" "$QUERIES" | tee "$OUT/sqlite.json"
./bench/tantivy-bench/target/release/tantivy-bench "$CORPUS" "$QUERIES" --iters 20 | tee "$OUT/tantivy.json"
snapshot after
echo "trial done: $OUT"
