# Benchmarks: methodology and raw results (v0.2.0)

All numbers below were measured on this machine. The query set
(`tools/bench_queries.txt`) and the benchmark scripts were frozen before
the v0.1.0 runs and have not been touched since; the v0.2.0 work re-ran the
identical benchmark (plus the same manual merged-index steps) in full:
3 trials before, 3 trials after, medians reported.
Raw JSON: `bench/results/before/` and `bench/results/after/`
(`median.json` + per-run files incl. load snapshots).

## Machine and noise

- 4 × Intel Xeon E5-2699 v4 @ 2.20 GHz, 8 GB RAM, `rustc 1.99.0`, release.
- Shared VPS: per-trial load averages ranged 0.2–1.6 (other tenants
  active; each run's `load_before.txt`/`load_after.txt` records it).
  Mitigation: 20 iterations per query (p50/p95/p99), median of 3 full
  trials. Single-digit-% deltas should be read as noise; all reported
  v0.2.0 wins are 7× or more.

## Corpus

100,000 documents from Simple English Wikipedia
(`simplewiki-latest-pages-articles.xml.bz2`, CC BY-SA 3.0 / GFDL —
attribution: Wikipedia contributors, via dumps.wikimedia.org),
parsed to `{title, body}` JSON lines, 140 MB raw, byte-identical across
rounds (139,970,137 bytes). Generator: frozen `tools/make_corpus.py`.
The corpus itself is **not** committed; regenerate with the script.

## Indexing throughput (100k docs, median of 3)

| engine | time | docs/s | index size |
|---|---|---|---|
| v0.1.0 | 183.4 s | 545 | 60 MB inverted + 141 MB stored + 138 MB WAL |
| v0.2.0 | 137.7 s | 726 (1.33×) | 60 MB inverted + 141 MB stored + **0 MB WAL** |
| sqlite-fts5 | 11.4 → 11.6 s | ~8,700 | 249 MB |
| tantivy 0.24 (in-RAM) | 3.40 → 3.42 s | ~29,300 | n/a (in-RAM bench) |

(sqlite/tantivy re-measured each round as a noise control — unchanged, as
expected for untouched engines.) WAL checkpoint truncation removed the
whole 138 MB log; merges run 184.1 → 141.6 s (1.30×, 99 tiered rounds).
Peak RSS during index+merge+bench was ~4.5 GB (indicative, 1 Hz sampling);
search alone peaks at 1.6 GB (decoded postings + positions resident).

Why indexing is still slower: analysis + map inserts dominate (measured:
`add()` is 7 µs/doc; `flush()` was 1.45 ms/doc). Parallel flush/merge
bought 1.3× on 4 cores; the rest is serial sort/encode/fsync plus
per-token Rust overhead vs batched C writers. WAL `write_all` per doc is
~0.1% of index time, so no batching was added — fsync stays once per flush
(group commit where it matters).

## Query latency (merged 1-segment index, top-10, p50/p95/p99 ms, 8 threads)

Single-thread is within ±5% on one segment (see `*_st.json`).

| query | type | v0.1.0 p50 | v0.2.0 p50 | speedup | sqlite | tantivy |
|---|---|---|---|---|---|---|
| `history` | term | 106.1 | 0.78 | 135× | 0.29 | 0.39 |
| `city river` | free text¹ | 149.7 | 0.70 | 214× | 0.20 | 0.68 |
| `king AND queen` | AND | 141.4 | 1.01 | 140× | 0.14 | 0.47 |
| `king OR queen` | OR | 145.8 | 0.33 | 446× | 0.08 | 0.32 |
| `"united states"` | phrase | 1268.0 | 7.99 | 159× | 0.15 | 2.53 |
| `"world war"` | phrase | 200.1 | 1.47 | 136× | 0.14 | 0.62 |
| `hist*` | prefix | 291.5 | 1.37 | 213× | 1.83 | 0.04 |
| `histroy~1` | fuzzy | 428.7 | 59.1 | 7× | n/a (0 hits) | 0.02 (0 hits) |
| `title:history` | field | 62.6 | 0.09 | 680× | 0.10 | 0.01 |
| `(king OR queen) AND england` | nested | 191.5 | 1.99 | 96× | 0.17 | 0.56 |
| `NOT war` | NOT | 122.7 | 7.62 | 16× | syntax err | parse err |
| `england` | term | 105.4 | 0.58 | 182× | 0.05 | 0.13 |

¹ `city river` changed AND→OR semantics by design (free-text default);
same string, intendedly different matching set.

Unmerged (100 segments) p50 also improved across the board, e.g. `history`
116.3 → 2.29 ms, `"world war"` 994.6 → 2.94 ms, `histroy~1` 1538.6 →
282.3 ms — per-segment overhead is gone from stats, not just eval.

What changed (all committed before the after-runs): per-segment field
totals cached at build/load/merge (avg in O(segments)); df via posting
lengths with a no-deletes fast path; prefix range scans on the dict block
index; fuzzy length buckets + scratch-buffer edit distance; dense
per-segment accumulators with hoisted idf; galloping phrase intersection
returning posting indices; real MaxScore WAND for root-level term
disjunctions; `select_nth` top-k. Decoded-resident postings were deliberately
kept, so block-decode + skip-pointer traversal was skipped as pointless
here (documented in ARCHITECTURE.md).

Where we still lose: term/OR queries run within ~1–5× of Tantivy
(`city river`
0.70 vs 0.68 ms; `king OR queen` 0.33 vs 0.32 ms; `history` 0.78 vs
0.39 ms; `england` 0.58 vs 0.13 ms), while SQLite stays
10–100× faster on most queries and prefix (`hist*` 1.37 vs 0.04 ms),
fuzzy (`histroy~1` 59 ms) and `NOT`
complement scoring (7.6 ms) remain weak spots — edit distance over buckets
instead of an automaton, two-pass phrase scoring, hash cross-segment
merges. All stated plainly; oracle parity held throughout (48 tests).

## Relevance (BEIR SciFact, raw queries, no rewrite)

- Dataset: BEIR SciFact test, 5,183 docs / 300 queries, downloaded from
  https://public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets/scifact.zip
  (BEIR: Thakur et al., https://github.com/beir-cellar/beir).
- v0.1.0 (implicit AND): **nDCG@10 = 0.007, Recall@100 = 0.007**
  (`bench/results/before/relevance.json`, frozen `tools/eval_beir.py`).
- v0.2.0 (free-text default OR, `minimum_should_match = 1`):
  **nDCG@10 = 0.480, Recall@100 = 0.654**
  (`bench/results/after/relevance.json`, same frozen script).
- Published baseline, fetched and cited: BM25 **0.620** nDCG@10 on SciFact
  (Thakur et al., “BEIR: A Heterogeneous Benchmark for Zero-shot
  Evaluation of Information Retrieval Models”, arXiv:2104.08663v2,
  Table 2: “All scores denote nDCG@10”). Remaining gap: different
  analysis/tokenization, 67/300 queries still empty (query terms absent
  from corpus vocabulary), no reranking. The raw default (0.480) scores
  below the old eval-time OR-variant (0.514) because the rewrite ORs every
  whitespace token while the parser reads English "or"/"and"/"not" inside
  claims (44/20/6 of the 300 test queries) as boolean syntax, forcing those
  queries down the strict path with different candidate sets. The old
  eval-time OR-variant
  (0.514, `tools/eval_beir_or.py`) is superseded and kept only for history.

Charts: `docs/latency.svg`, `docs/comparison.svg`, `docs/throughput.svg`
(refreshed), `docs/latency_before_after.svg` (log scale) — hand-written SVG
via `tools/charts_before_after.py`; frozen `tools/charts.py` untouched.
