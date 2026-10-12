# Learning guide

Glossary first, then 20 interview-style questions and answers about this
codebase. Short answers — the code is the long answer.

## Glossary

- **Token**: smallest searchable unit (`analysis::tokenize`).
- **Term**: dictionary entry — an analyzed token (`field → term`).
- **Posting**: one `(doc, freq, positions)` record in a term's list.
- **Posting list**: all postings of one term, doc-sorted, delta+varint coded.
- **Inverted index**: map from terms to posting lists (vs forward: doc→terms).
- **Segment**: one immutable inverted index file + in-memory form.
- **WAL**: write-ahead log — mutations land here before memory.
- **Tombstone**: a delete marker; dead docs vanish at merge time.
- **Merge**: combining segments into fewer, bigger ones.
- **Snapshot**: a consistent, Arc-shared view of segments for one search.
- **BM25**: probabilistic ranking with tf saturation + length normalization.
- **df / idf**: document frequency / inverse df — rare terms weigh more.
- **Top-k**: keep only the k best hits.
- **Phrase query**: terms that must appear consecutively (positions).
- **Prefix query**: `term*` expands over the term dictionary.
- **Fuzzy query**: `term~N` matches dict terms within edit distance N.
- **Stemming**: reducing words to a root (`running`→`run`).
- **Stop words**: frequent words dropped at index time (`the`, `and`…).
- **Skip pointer**: jump index inside long posting lists.
- **Front coding**: dictionary compression via shared prefixes.
- **Varint**: variable-length integer code (7 bits/byte).
- **Oracle**: the brute-force reference engine tests compare against.

## 20 questions

1. **Why immutable segments?** Writers never mutate what readers see, so
   search needs no locks — a snapshot is just a `Vec<Arc<Segment>>` clone.
2. **Where does durability come from?** The WAL: every mutation is appended
   (length-prefixed binary record) before in-memory state changes; the
   manifest checkpoints which prefix is already in segments.
3. **What happens if we crash mid-merge?** The manifest still lists the old
   segments, so recovery ignores the half-written file; orphans are deleted
   on the next open. Tested via `FailAt::MergeWrite/MergeCommit`.
4. **How are deletes handled without rewriting segments?** Tombstones: the
   id leaves the `live` set; postings keep it until a merge physically drops
   it (`retain_live`).
5. **How is BM25's idf smoothed?** `ln(1 + (N−df+0.5)/(df+0.5))` — never
   negative, defined for df=0.
6. **Why delta + varint for doc ids?** Sorted ids have small gaps; gaps fit
   in 1–2 bytes instead of 4. Positions compress the same way.
7. **What do skip pointers accelerate?** AND-intersection: jump over blocks
   of a long list instead of stepping posting by posting.
8. **How does the term dictionary save space?** Front coding in 16-key
   blocks: shared prefixes stored once (2.7 MB for the wiki vocab).
9. **Why keep term positions?** Phrase queries need adjacency, which
   doc-level postings can't answer.
10. **How are phrase queries scored?** `occ` = number of phrase occurrences
    acts as tf in BM25; df = docs containing the whole phrase (global,
    exact — including repeated-term phrases like `"a a"`).
11. **Term vs OR-variant BEIR gap (0.007 → 0.514)?** Implicit AND
    over-constrained verbose queries; ranking was fine, candidate generation
    was the gap. v0.2.0 made default-OR the engine behavior (raw queries now
    score 0.480 nDCG@10 vs published BM25 0.620).
12. **How do prefix queries work?** Range scan on the front-coded dict via
    the block index (seek + early stop, never a full decode), then BM25 over
    expanded terms (summed).
13. **How do fuzzy queries work?** Length buckets restrict candidates to
    compatible lengths, then Levenshtein with early exit and reused row
    buffers (cap 2), then BM25 over matches.
14. **What does `QueryStats` precompute and why?** Expansion union, df,
    avg field length, phrase df — once per search, because per-segment
    recomputation was quadratic in segment count (measured 4.5× win).
15. **Why did snapshots switch to `Arc` for stored fields?** Cloning 140 MB
    of stored text per query cost ~150 ms; Arc + copy-on-write made
    snapshots O(segments).
16. **How is search multi-threaded?** `std::thread::scope`, one chunk of
    segments per thread (capped by `SearchOptions::max_threads`), results
    merged and top-k truncated deterministically.
17. **How do property tests work without proptest?** A 10-line xorshift
    `Rng` (std only) generates corpora + queries; the engine must match the
    brute-force oracle's set/order exactly and scores within 1e-3 relative.
18. **What does the merge-equivalence test prove?** Same query before/after
    `merge_all` returns identical hits — merging never loses or distorts.
19. **Why is SQLite still 10–100× faster per query?** Block-coded lists,
    mature C loops, no per-query setup vs our snapshot + stats + hash
    merges. The gap narrowed from ~1000× to ~10–100× (term queries now
    match Tantivy); fuzzy and complement scoring remain weak. All measured,
    all reported.
20. **Biggest honest weakness?** RAM: decoded postings + positions resident
    (1.6 GB per 100k wiki docs); merging 100 segments to one peaked at
    ~4.5 GB. Fix direction: block-decoded postings.

## v0.2.0 addendum

21. **What did profiling actually find?** Eval was already ~ms; stats setup
    dominated (df scans, a 1.29 s all-terms enumeration, 39 ms field scans).
    Fix the measured hog, not the suspected one.
22. **What was the WAND truncation bug?** Pruning an `Or` to top-k inside
    `And` composition dropped docs the parent needed (`(king OR queen) AND
    england` went 10 hits → 1). Rule: truncate only at the query root.
23. **What was the WAL resurrection bug?** Truncating the whole log dropped
    DELs of still-stored docs, resurrecting them on reopen. Rule: veto
    truncation while any tombstone is still stored; preserve the tail.
24. **Why no block-decode + skip traversal?** Postings are decoded-resident,
    where gallop/binary-search is already optimal — a decode layer would add
    machinery for zero gain. Skips stay maintained and tested.
25. **How does `minimum_should_match` count?** Per free-text part (a
    multi-term prefix/fuzzy part counts once), summed scores like OR,
    engine and oracle identical.
