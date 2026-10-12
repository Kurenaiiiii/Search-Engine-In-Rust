# Changelog

## v0.2.0 — query speed, indexing, default-OR, WAL truncation

All numbers are medians of 3 full benchmark runs (20 iterations/query) on
4× Xeon E5-2699 v4 / 8 GB RAM, 100k SimpleWiki docs; see docs/BENCHMARKS.md.
48 tests pass (41 unit + 4 oracle property suites + 3 server).

- Query latency: 96–680× faster across 11/12 frozen queries (term
  `history` 106.1 → 0.78 ms; `king OR queen` 145.8 → 0.33 ms;
  `"united states"` 1268 → 8.0 ms; `title:history` 62.6 → 0.09 ms;
  fuzzy `histroy~1` 428.7 → 59.1 ms). Term/OR queries run within ~1–5× of
  Tantivy (e.g. `king OR queen` 0.33 vs 0.32 ms; `history` 0.78 vs 0.39 ms;
  `england` 0.58 vs 0.13 ms).
- How: cached per-segment field totals + no-deletes fast paths, dict block
  range scans, fuzzy length buckets + scratch-buffer edit distance, dense
  per-segment accumulators with hoisted idf, galloping phrase intersection
  returning posting indices, real MaxScore WAND (root-level flat
  disjunctions only), `select_nth` top-k.
- Indexing: 183.4 → 137.7 s (545 → 726 docs/s); merges 184.1 → 141.6 s via
  parallel flush builds, parallel/streaming merge (no input clones,
  inline tombstone filtering), single-pass skip tables.
- WAL checkpoint truncation: index dir 339 → ~201 MB (WAL 138 MB → 0);
  vetoed while tombstones are still stored (resurrection bug caught by the
  new tests); crash-safe staged protocol.
- Query semantics: free text defaults to scored OR with
  `minimum_should_match` (CLI `--minimum-should-match`, API `msm`);
  operators/quotes/parens/fields stay strict. BEIR SciFact raw queries:
  nDCG@10 0.007 → 0.480, Recall@100 0.007 → 0.654 (published BM25 baseline
  0.620, Thakur et al., arXiv:2104.08663v2 Table 2). The raw default scores
  below the old eval-time OR rewrite (0.514) because the rewrite ORs every
  whitespace token while the parser reads English "or"/"and"/"not" inside
  claims (44/20/6 of the 300 test queries) as boolean syntax, forcing those
  queries down the strict path with different candidate sets.
- Still slower than SQLite (10–100×/query) and Tantivy on prefix/fuzzy;
  decoded-resident postings keep RAM at 1.6 GB per 100k docs.

## v0.1.0 — initial release

From-scratch BM25 engine (std-only core), HTTP API + web UI, CLI,
WAL durability, tiered merges, brute-force oracle property tests,
BEIR/SciFact + SQLite + Tantivy benchmarks with honest gaps reported.
