# Search Engine in Rust

[![ci](https://github.com/Kurenaiiiii/Search-Engine-In-Rust/actions/workflows/ci/badge.svg)](https://github.com/Kurenaiiiii/Search-Engine-In-Rust/actions/workflows/ci)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](LICENSE-MIT)

A from-scratch full-text search engine (a mini Lucene) in Rust: BM25 ranking,
boolean/phrase/prefix/fuzzy queries, segments with a write-ahead log, an
HTTP JSON API with a web UI, and a CLI. `se-core` is **zero dependencies**.

## Quick start

```bash
cargo build --release -p se-cli -p se-server
printf '%s\n' \
  '{"title":"Rust search engine","body":"A fast full text search engine written in Rust"}' \
  '{"title":"Cooking pasta","body":"Simple pasta recipes for dinner"}' \
  > docs.ndjson
./target/release/se-cli --dir ./data index docs.ndjson
./target/release/se-cli --dir ./data search "rust AND engine"
./target/release/se-cli --dir ./data serve  # same binary serves the API + UI on :3000
```

Real output:

```
indexed 3 documents in 0.00s (4519 docs/s)
3 hits in 0 ms for: rust
  #2      0.6425  Rust programming
           Learn <mark>Rust</mark> ownership borrowing and lifetimes
```

Or run the server directly (`SE_TOKEN=secret` enables bearer auth):

```bash
SE_DATA=./data ./target/release/se-server --addr 127.0.0.1:3000
curl 'localhost:3000/api/search?q="search engine"&highlight=1'
```

Open `http://localhost:3000/` for the web UI (search box, highlighted
snippets, latency, stats panel — no external assets).

## Features

- Analysis: Unicode tokenizer, stop words, hand-written Porter-style
  stemmer, edge n-grams; per-field analyzers and boosts.
- Index: immutable segments, delta+varint postings with skip pointers,
  term positions, front-coded term dictionary, stored fields, write buffer,
  WAL with crash recovery, tombstone deletes/updates, tiered background
  merges, snapshot readers, multi-threaded search.
- Ranking: BM25 (default) + TF-IDF option, top-k heap, field and query
  boosts.
- Queries: `AND OR NOT`, parens, `"phrases"`, `field:term`, `prefix*`,
  `term~N`, `term^boost`, with clear error messages.
- Extras: `<mark>` snippets, autocomplete suggest, did-you-mean.
- Server: create/bulk/get/update/delete/search/suggest/spell/stats,
  bearer auth, 8 MiB body cap, structured logs, Dockerfile + compose.

## Benchmarks (real, on 4× Xeon E5-2699 v4 / 8 GB RAM)

100k Simple English Wikipedia docs (140 MB raw, CC BY-SA 3.0/GFDL —
Wikipedia contributors via dumps.wikimedia.org). Full methodology and raw
JSON in [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) and `bench/results/`.

| engine | index time | docs/s | index size |
|---|---|---|---|
| this engine | 185 s | 540 | 60 MB inverted + 141 MB stored + 138 MB WAL |
| sqlite-fts5 | 11 s | 9,009 | 249 MB |
| tantivy 0.24 | 3.3 s | 30,303 | n/a (in-RAM) |

![throughput](docs/throughput.svg)

Query latency p50 (ms, top-10, merged index):

| query | ours | sqlite | tantivy |
|---|---|---|---|
| `history` | 98.1 | 0.29 | 0.24 |
| `king OR queen` | 135.0 | 0.09 | 0.21 |
| `"world war"` (phrase) | 189.2 | 0.14 | 0.62 |
| `hist*` (prefix) | 275.2 | 1.82 | 0.02 |
| `histroy~1` (fuzzy) | 420.0 | n/a | 0.01 |

![latency](docs/latency.svg)
![comparison](docs/comparison.svg)

Relevance (BEIR SciFact, 300 queries): raw queries nDCG@10 **0.007**
(implicit AND over-constrains verbose queries) vs OR-variant nDCG@10
**0.514**, Recall@100 **0.701** (eval-time rewrite, engine unchanged). No
published baseline is quoted — none could be fetched from a citable source.

We report plainly where we lose: per-query latency is ~100–1000× behind
SQLite/Tantivy (per-query snapshot + stat scans, `HashMap` score
accumulation, no WAND pruning yet) and RAM is high (1.6 GB RSS per 100k
docs). Correctness (41 tests incl. a brute-force oracle) came first.

## Limitations

- WAL is never truncated (dir grows by raw size); checkpoint truncation is
  roadmap.
- Merging 100 segments to one peaked at 5.8 GB RAM on this box.
- No SHOULD/minimum-should-match for verbose queries; no distributed
  search; single-writer index; auth is a single shared bearer token.

## Roadmap

WAL truncation, block-decoded postings (RAM), WAND/MaxScore pruning, SHOULD
clauses, incremental field stats, group commit, multi-index sharding.

## Structure

```
se-core/    engine (std only): analysis codec segment query search index brute
se-server/  HTTP API + static UI (std only, no axum/tokio by design)
se-cli/     index / search / stats / suggest / bench / optimize / serve
tools/      frozen bench harness (queries, scripts) + eval + SVG charts
docs/       ARCHITECTURE.md LEARNING.md API.md BENCHMARKS.md + charts
bench/results/  raw benchmark JSON (committed)
```

## Docs

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — inverted indexes,
  compression byte examples, BM25 worked example, WAL, trade-offs.
- [`docs/LEARNING.md`](docs/LEARNING.md) — glossary + 20 interview Q&As.
- [`docs/API.md`](docs/API.md) — curl examples from real calls.
- [`docs/BENCHMARKS.md`](docs/BENCHMARKS.md) — methodology + raw results.

## License

MIT OR Apache-2.0. Benchmark corpus: Simple English Wikipedia (CC BY-SA 3.0
/ GFDL, © Wikipedia contributors).
