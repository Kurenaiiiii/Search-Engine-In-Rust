# Architecture

Search Engine in Rust is a mini-Lucene: a persistent inverted index with
BM25 ranking, exposed through a library (`se-core`), an HTTP server
(`se-server`) and a CLI (`se-cli`). Everything in `se-core` is hand-written
with zero dependencies.

```
                         ┌──────────────┐
  docs ──► analyze ──►   │ write buffer │──flush──► immutable segments
   │                     │  + WAL (log) │           (inverted index each)
   │                     └──────────────┘                    │
   │                              tombstones for deletes    ▼
   └──────────────────► snapshots (Arc-shared, consistent) ──► search
                                                                 (BM25)
```

## 1. Analysis (`se-core/src/analysis.rs`)

Unicode-aware tokenizer (maximal runs of alphanumeric chars, lowercase
folding), English stop-word removal (positions keep a gap so phrases still
work), a hand-written Porter-style stemmer, optional edge n-grams for
autocomplete. Per-field configuration via `Schema`
(`analyzer_for` / `boost_for`).

## 2. Postings compression (`codec.rs`) — with real bytes

- **Variable-byte codes**: 7 payload bits per byte, high bit = last byte.
  Example from the test suite: `300` encodes as `[0x2C, 0x82]`
  (`300 = 0b10_0101100` → low 7 bits `0101100` + stop bit, then `10`).
- **Delta encoding**: sorted doc ids `[1, 2, 3, 100]` become gaps
  `[1, 1, 1, 97]` before varint coding — small gaps dominate, so postings
  shrink ~4× vs raw u32s.
- **Full postings**: per posting `doc_gap freq npos pos_gap…`; positions use
  `+1` offset coding so a position of 0 stays encodable.
- **Skip pointers**: one entry per 8 postings `(index, byte_offset, doc_id)`,
  used to jump inside long lists during intersection.

## 3. Term dictionary — prefix (front) coding (`segment.rs`)

Sorted `field\0term` keys are stored in blocks of 16: the first key whole,
then `(shared_prefix_len, suffix)` pairs. 100 front-coded blocks carry a
sparse index for binary search; `contains` / `scan_prefix` decode at most
one block. Measured: 100k-doc wiki vocab compresses to **2.7 MB**.

## 4. Segments — immutable + mergeable

A `Segment` holds postings keyed by `(field, term)` with *local* doc ids, a
stored-fields document store, and per-doc field lengths for norms.
`SegmentWriter` collects the write buffer; `finish()` freezes it forever —
immutability is what makes lock-free snapshot search possible.

Tiered merging (`merge_tier`, 2 smallest segments at a time; `merge_all` for
small indexes; `se-cli optimize` loops the tier policy) unions doc lists
sorted by global id (good delta compression), remaps local ids with a
sort-based pass, and drops tombstoned docs. Merging 100×1k-doc wiki segments
yielded one 60 MB inverted index from 75 MB of fragments. Flush builds and
merge gathers parallelize analysis and per-term work across up to 4 threads
(`build_segment_parallel`, scoped term slices); skip tables are walked
straight from the encoded bytes so flush/merge never pay a second encode
pass. Each segment also caches per-field token totals and term-length
buckets at build/load/merge time, so queries never rescan documents for
statistics.

## 5. BM25 — worked numeric example

`score = idf · (tf·(k1+1)) / (tf + k1·(1−b+b·dl/avg))`,
`idf = ln(1 + (N−df+0.5)/(df+0.5))`, defaults `k1=1.2, b=0.75`.

Take `tf=2, dl=100, avg=120, df=5, N=100000`:
`idf = ln(1 + 99995.5/5.5) = 9.8082`,
`norm = 0.25 + 0.75·(100/120) = 0.875`,
`score = 9.8082 · 4.4/(2 + 1.05) = 14.1495`
— exactly what `bm25()` returns. Per-field boosts multiply the term weight;
TF-IDF (`(1+ln tf)·ln(1+N/(df+1))`) is a `--scorer`/`scorer=` option.
The idf factor is hoisted out of per-posting loops (bit-identical results).
Scoring writes into dense per-segment accumulators (local ids are dense);
cross-segment hits merge by hash only when needed; top-k uses
`select_nth` + a final sort with deterministic `(score desc, doc asc)`
order. Expansion (prefix/fuzzy), df and phrase-df are computed once per
search in `QueryStats`, not once per segment. Root-level flat term
disjunctions run MaxScore WAND pruning (exact: pruned docs provably cannot
enter the top-k); nested queries use full-set dense evaluation so `And`/`Not`
parents stay correct — a truncation-inside-composition bug caught in testing
is documented in LEARNING.md.

## 6. Durability — WAL (`index.rs`)

Every mutation appends a length-prefixed binary record (`ADD`/`DEL`) to
`wal.log` before touching memory. Flush writes the new segment file, fsyncs,
then atomically rewrites `MANIFEST` (segment list + `wal_applied` offset).
After each flush a checkpoint drops the covered log prefix while preserving
any uncheckpointed tail (staged aside, offset reset, atomic rename); the
checkpoint is vetoed while a tombstoned doc is still stored, since its DEL
is the only durable copy — dropping it would resurrect the doc (both the
bug and the veto are covered by tests).
Recovery = load manifest segments + replay newer WAL records. Crash points
are covered by tests at flush-write, flush-commit, merge-write and
merge-commit. Files in `segments/` not listed in the manifest (crashed-merge
leftovers) and staged `wal.log.tmp` files are deleted on open. Measured:
`add()` costs 7 µs/doc (WAL write included), so there is no write batching —
fsync stays once per flush, which is where group commit already happens.

## 7. Query language (`query.rs`)

Hand-written lexer + recursive-descent parser (`OR` lowest, `AND`/`NOT`
above it, atoms at the bottom). Pure free text (bare terms with no
operators, quotes, parens or fields) parses to `Should` — a scored
disjunction with `minimum_should_match`; anything explicit parses strict.
Every syntax error names the problem and the fix, e.g. `missing closing
')'`, `edit distance 5 too large: max is 2`, `empty prefix: need a
character before *`.

## 8. Design trade-offs (honest)

- **Decoded postings resident in RAM** (1.6 GB RSS for 100k wiki docs):
  fast iteration, huge footprint. With resident decoded lists, gallop via
  binary search is already optimal, so block-decode + skip-pointer
  traversal was deliberately skipped (skips are still maintained, tested,
  and ready for an on-disk layout that would cut RAM ~10× at some CPU cost).
- **MaxScore WAND only at the query root** for flat term disjunctions:
  truncating inside `And`/`Not` composition would corrupt results (caught
  by testing); nested queries pay full dense evaluation.
- **Per-segment field totals cached** at build/load/merge (plus a
  no-deletes fast path) instead of per-query scans; deletes fall back to
  exact scans. Same for df.
- **Single-writer WAL with fsync per flush**: durable; indexing measured at
  726 docs/s on 4 cores (parallel flush/merge builds bought 1.3×).
- **Merges are 2-smallest-at-a-time, streaming** (borrowed inputs, inline
  tombstone filtering, parallel term gather): full 100→1 merge peaked at
  ~4.5 GB here vs 5.8 GB before, with identical results (merge-equivalence
  tests).
