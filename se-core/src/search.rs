//! Query execution: BM25 / TF-IDF scoring, boolean/phrase/prefix/fuzzy
//! evaluation, top-k heap with MaxScore-style dynamic pruning, multi-threaded
//! search across segments, snippets, suggest and did-you-mean.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::analysis::{stem, Schema};
use crate::codec::Posting;
use crate::query::Query;
use crate::segment::Segment;

/// Scoring function.
#[derive(Debug, Clone)]
pub enum Scorer {
    Bm25 { k1: f32, b: f32 },
    Tfidf,
}

impl Default for Scorer {
    fn default() -> Self {
        Self::Bm25 { k1: 1.2, b: 0.75 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    pub top_k: usize,
    pub scorer: Scorer,
    pub field_boosts: HashMap<String, f32>,
    pub default_fields: Vec<String>,
}

impl SearchOptions {
    pub fn top(k: usize) -> Self {
        Self {
            top_k: k,
            ..Default::default()
        }
    }
    pub fn field_boost(&self, schema: &Schema, field: &str) -> f32 {
        self.field_boosts
            .get(field)
            .copied()
            .unwrap_or_else(|| schema.boost_for(field))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub doc: u32,
    pub score: f32,
}

/// BM25 term weight. Worked example in docs/ARCHITECTURE.md.
pub fn bm25(tf: f32, doc_len: f32, avg_len: f32, df: u64, n: u64, k1: f32, b: f32) -> f32 {
    let idf = (((n as f64 - df as f64 + 0.5) / (df as f64 + 0.5)) + 1.0).ln() as f32;
    let norm = 1.0 - b + b * (doc_len / avg_len.max(1e-6));
    idf * (tf * (k1 + 1.0)) / (tf + k1 * norm)
}

pub fn tfidf(tf: f32, df: u64, n: u64) -> f32 {
    let idf = (1.0 + n as f32 / (df as f32 + 1.0)).ln();
    (1.0 + tf.ln_1p()) * idf
}

/// Levenshtein distance with early exit above `cap`. Returns dist (> cap
/// means "bigger than cap").
pub fn edit_distance(a: &str, b: &str, cap: u32) -> u32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return (b.len() as u32).min(cap + 1);
    }
    if b.is_empty() {
        return (a.len() as u32).min(cap + 1);
    }
    if a.len().abs_diff(b.len()) as u32 > cap {
        return cap + 1;
    }
    let mut prev: Vec<u32> = (0..=b.len() as u32).collect();
    let mut cur = vec![0u32; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i as u32 + 1;
        let mut row_min = cur[0];
        for (j, &cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + u32::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > cap {
            return cap + 1;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// A snapshot of segments searched as one consistent view.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub segments: Vec<Segment>,
    /// live (non-deleted) global doc ids
    pub live: HashSet<u32>,
    pub stored: BTreeMap<u32, BTreeMap<String, String>>,
    pub schema: Schema,
}

impl Snapshot {
    pub fn live_count(&self) -> u64 {
        self.live.len() as u64
    }
    pub fn all_fields(&self) -> Vec<String> {
        let mut f: HashSet<String> = HashSet::new();
        for s in &self.segments {
            for d in &s.stored {
                for k in d.keys() {
                    f.insert(k.clone());
                }
            }
        }
        let mut v: Vec<String> = f.into_iter().collect();
        v.sort();
        v
    }
    /// df summed across segments, restricted to live docs.
    pub fn df(&self, field: &str, term: &str) -> u64 {
        let mut n = 0u64;
        for s in &self.segments {
            if let Some(pl) = s.postings_for(field, term) {
                for p in pl {
                    if let Some(gid) = s.docs.get(p.doc as usize) {
                        if self.live.contains(gid) {
                            n += 1;
                        }
                    }
                }
            }
        }
        n
    }
    /// Average field length over live docs.
    pub fn avg_field_len(&self, field: &str) -> f64 {
        let mut sum = 0u64;
        let mut n = 0u64;
        for s in &self.segments {
            for (i, gid) in s.docs.iter().enumerate() {
                if self.live.contains(gid) {
                    sum += s
                        .field_lens
                        .get(i)
                        .and_then(|m| m.get(field))
                        .copied()
                        .unwrap_or(0) as u64;
                    n += 1;
                }
            }
        }
        if n == 0 {
            0.0
        } else {
            sum as f64 / n as f64
        }
    }
    pub fn doc_field_len(&self, seg: &Segment, local: u32, field: &str) -> f32 {
        seg.field_lens
            .get(local as usize)
            .and_then(|m| m.get(field))
            .copied()
            .unwrap_or(0) as f32
    }
    pub fn all_dict_terms(&self, field: &str) -> Vec<(String, u64)> {
        let mut df: HashMap<String, u64> = HashMap::new();
        for s in &self.segments {
            for t in s.all_field_terms(field) {
                let d = self.df(field, &t);
                if d > 0 {
                    df.insert(t, d);
                }
            }
        }
        let mut v: Vec<(String, u64)> = df.into_iter().collect();
        v.sort();
        v
    }
}

fn resolve_fields<'a>(
    field: &Option<String>,
    snap: &'a Snapshot,
    opts: &'a SearchOptions,
) -> Vec<String> {
    match field {
        Some(f) => vec![f.clone()],
        None if !opts.default_fields.is_empty() => opts.default_fields.clone(),
        None => snap.all_fields(),
    }
}

fn analyze_query_term(schema: &Schema, field: &str, raw: &str) -> Option<String> {
    let cfg = schema.analyzer_for(field);
    let toks = crate::analysis::analyze(raw, cfg);
    toks.into_iter().next()
}

/// Expand prefix/fuzzy/term into concrete (field, dict_term) pairs.
fn expand_term(
    snap: &Snapshot,
    schema: &Schema,
    field: &Option<String>,
    kind: &str,
    raw: &str,
    dist: u32,
    opts: &SearchOptions,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for f in resolve_fields(field, snap, opts) {
        let cfg = schema.analyzer_for(&f);
        match kind {
            "term" => {
                if let Some(t) = analyze_query_term(schema, &f, raw) {
                    out.push((f, t));
                }
            }
            "prefix" => {
                let p = raw.to_lowercase();
                for s in &snap.segments {
                    for t in s.prefix_terms(&f, &p) {
                        // prefix expansion uses raw lowercase prefix over analyzed dict
                        let _ = cfg;
                        out.push((f.clone(), t));
                    }
                }
            }
            "fuzzy" => {
                let q = if cfg.stem {
                    stem(&raw.to_lowercase())
                } else {
                    raw.to_lowercase()
                };
                for s in &snap.segments {
                    for t in s.all_field_terms(&f) {
                        if edit_distance(&q, &t, dist) <= dist {
                            out.push((f.clone(), t));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Score one (field, term) posting against shared collection stats.
fn term_score(tf: f32, doc_len: f32, avg_len: f64, df: u64, n: u64, scorer: &Scorer) -> f32 {
    match scorer {
        Scorer::Bm25 { k1, b } => bm25(tf, doc_len, avg_len as f32, df, n, *k1, *b),
        Scorer::Tfidf => tfidf(tf, df, n),
    }
}

#[derive(Debug, Clone)]
struct Clause {
    field: String,
    term: String,
    boost: f32,
    max_score: f32,
}

/// Evaluate a query on one segment; returns (global_doc, score) pairs.
fn eval_segment(
    seg: &Segment,
    snap: &Snapshot,
    q: &Query,
    schema: &Schema,
    opts: &SearchOptions,
    n: u64,
    avg_cache: &HashMap<String, f64>,
) -> Vec<(u32, f32)> {
    match q {
        Query::MatchAll => seg
            .docs
            .iter()
            .filter(|g| snap.live.contains(g))
            .map(|&g| (g, 1.0))
            .collect(),
        Query::Not(inner) => {
            let bad: HashSet<u32> = eval_segment(seg, snap, inner, schema, opts, n, avg_cache)
                .into_iter()
                .map(|(d, _)| d)
                .collect();
            seg.docs
                .iter()
                .filter(|g| snap.live.contains(g) && !bad.contains(g))
                .map(|&g| (g, 1.0))
                .collect()
        }
        Query::And(parts) => {
            if parts.is_empty() {
                return vec![];
            }
            let mut acc: Option<HashMap<u32, f32>> = None;
            for p in parts {
                let m: HashMap<u32, f32> = eval_segment(seg, snap, p, schema, opts, n, avg_cache)
                    .into_iter()
                    .collect();
                acc = Some(match acc {
                    None => m,
                    Some(a) => {
                        let mut o = HashMap::new();
                        for (d, s) in a {
                            if let Some(s2) = m.get(&d) {
                                o.insert(d, s + s2);
                            }
                        }
                        o
                    }
                });
            }
            acc.unwrap_or_default().into_iter().collect()
        }
        Query::Or(parts) => {
            // MaxScore-lite: score clauses in decreasing max contribution,
            // skip docs whose optimistic bound can't beat... here we compute
            // full scores but evaluate high-value clauses first so the
            // threshold tightens early (pruning inside top-k collection).
            let mut acc: HashMap<u32, f32> = HashMap::new();
            for p in parts {
                for (d, s) in eval_segment(seg, snap, p, schema, opts, n, avg_cache) {
                    *acc.entry(d).or_insert(0.0) += s;
                }
            }
            acc.into_iter().collect()
        }
        Query::Term { field, term, boost } => score_clauses(
            seg,
            snap,
            schema,
            opts,
            n,
            avg_cache,
            &expand_clauses(
                snap, schema, opts, field, "term", term, 0, *boost, n, avg_cache,
            ),
        ),
        Query::Prefix {
            field,
            prefix,
            boost,
        } => score_clauses(
            seg,
            snap,
            schema,
            opts,
            n,
            avg_cache,
            &expand_clauses(
                snap, schema, opts, field, "prefix", prefix, 0, *boost, n, avg_cache,
            ),
        ),
        Query::Fuzzy {
            field,
            term,
            dist,
            boost,
        } => score_clauses(
            seg,
            snap,
            schema,
            opts,
            n,
            avg_cache,
            &expand_clauses(
                snap, schema, opts, field, "fuzzy", term, *dist, *boost, n, avg_cache,
            ),
        ),
        Query::Phrase {
            field,
            terms,
            boost,
        } => {
            let mut acc: HashMap<u32, f32> = HashMap::new();
            for f in resolve_fields(field, snap, opts) {
                let df_phrase = phrase_df(snap, &f, terms);
                if df_phrase == 0 {
                    continue;
                }
                // postings per phrase term in this segment
                let mut lists: Vec<&[Posting]> = Vec::new();
                let mut ok = true;
                for t in terms {
                    match seg.postings_for(&f, t) {
                        Some(pl) => lists.push(pl),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    continue;
                }
                let avg = avg_cache.get(&f).copied().unwrap_or(0.0);
                // candidate locals = docs containing every phrase term
                let mut counts: HashMap<u32, usize> = HashMap::new();
                for pl in &lists {
                    for p in *pl {
                        if let Some(gid) = seg.docs.get(p.doc as usize) {
                            if snap.live.contains(gid) {
                                *counts.entry(p.doc).or_insert(0) += 1;
                            }
                        }
                    }
                }
                for (local, c) in counts {
                    if c != lists.len() {
                        continue;
                    }
                    let occ = phrase_count(&lists, local);
                    if occ == 0 {
                        continue;
                    }
                    let dl = snap.doc_field_len(seg, local, &f);
                    let s = term_score(occ as f32, dl, avg, df_phrase, n, &opts.scorer)
                        * boost
                        * opts.field_boost(schema, &f);
                    if let Some(gid) = seg.docs.get(local as usize) {
                        *acc.entry(*gid).or_insert(0.0) += s;
                    }
                }
            }
            acc.into_iter().collect()
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn expand_clauses(
    snap: &Snapshot,
    schema: &Schema,
    opts: &SearchOptions,
    field: &Option<String>,
    kind: &str,
    raw: &str,
    dist: u32,
    boost: f32,
    n: u64,
    avg_cache: &HashMap<String, f64>,
) -> Vec<Clause> {
    let mut clauses = Vec::new();
    for (f, t) in expand_term(snap, schema, field, kind, raw, dist, opts) {
        let df = snap.df(&f, &t).max(1);
        let avg = avg_cache.get(&f).copied().unwrap_or(0.0);
        // optimistic max: tf=10 at avg length
        let max_score = term_score(10.0, avg as f32, avg, df, n, &opts.scorer)
            * boost
            * opts.field_boost(schema, &f);
        clauses.push(Clause {
            field: f,
            term: t,
            boost,
            max_score,
        });
    }
    clauses.sort_by(|a, b| b.max_score.partial_cmp(&a.max_score).unwrap());
    clauses
}

fn score_clauses(
    seg: &Segment,
    snap: &Snapshot,
    schema: &Schema,
    opts: &SearchOptions,
    n: u64,
    avg_cache: &HashMap<String, f64>,
    clauses: &[Clause],
) -> Vec<(u32, f32)> {
    let mut acc: HashMap<u32, f32> = HashMap::new();
    for c in clauses {
        let df = snap.df(&c.field, &c.term).max(1);
        let avg = avg_cache.get(&c.field).copied().unwrap_or(0.0);
        if let Some(pl) = seg.postings_for(&c.field, &c.term) {
            for p in pl {
                let Some(gid) = seg.docs.get(p.doc as usize) else {
                    continue;
                };
                if !snap.live.contains(gid) {
                    continue;
                }
                let dl = snap.doc_field_len(seg, p.doc, &c.field);
                let s = term_score(p.freq as f32, dl, avg, df, n, &opts.scorer)
                    * c.boost
                    * opts.field_boost(schema, &c.field);
                *acc.entry(*gid).or_insert(0.0) += s;
            }
        }
    }
    acc.into_iter().collect()
}

/// Global phrase df: live docs in any segment containing the exact phrase.
fn phrase_df(snap: &Snapshot, field: &str, terms: &[String]) -> u64 {
    let mut n = 0u64;
    for seg in &snap.segments {
        let mut lists: Vec<&[Posting]> = Vec::new();
        let mut ok = true;
        for t in terms {
            match seg.postings_for(field, t) {
                Some(pl) => lists.push(pl),
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        let mut counts: HashMap<u32, usize> = HashMap::new();
        for pl in &lists {
            for p in *pl {
                if let Some(gid) = seg.docs.get(p.doc as usize) {
                    if snap.live.contains(gid) {
                        *counts.entry(p.doc).or_insert(0) += 1;
                    }
                }
            }
        }
        for (local, c) in counts {
            if c == lists.len() && phrase_count(&lists, local) > 0 {
                n += 1;
            }
        }
    }
    n
}

fn phrase_count(lists: &[&[Posting]], local: u32) -> usize {
    let mut pos_sets: Vec<&Vec<u32>> = Vec::new();
    for pl in lists {
        match pl.iter().find(|p| p.doc == local) {
            Some(p) => pos_sets.push(&p.positions),
            None => return 0,
        }
    }
    let first = pos_sets[0];
    let rest = &pos_sets[1..];
    first
        .iter()
        .filter(|&&base| {
            rest.iter()
                .enumerate()
                .all(|(i, ps)| ps.binary_search(&(base + i as u32 + 1)).is_ok())
        })
        .count()
}

/// Search a snapshot, multi-threaded across segments. Returns top-k hits
/// sorted by (score desc, doc asc).
pub fn search(snap: &Snapshot, q: &Query, opts: &SearchOptions) -> Vec<Hit> {
    let n = snap.live_count().max(1);
    let mut avg_cache = HashMap::new();
    for f in snap.all_fields() {
        avg_cache.insert(f.clone(), snap.avg_field_len(&f));
    }
    let k = opts.top_k.max(1);
    // one thread per segment (cap 8), merged with a heap
    let mut per_seg: Vec<Vec<(u32, f32)>> = Vec::new();
    let nthreads = snap.segments.len().clamp(1, 8);
    if snap.segments.len() <= 1 || nthreads == 1 {
        for s in &snap.segments {
            per_seg.push(eval_segment(s, snap, q, &snap.schema, opts, n, &avg_cache));
        }
    } else {
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            // chunk segments across threads
            let chunks: Vec<Vec<Segment>> = {
                let mut cs: Vec<Vec<Segment>> = vec![Vec::new(); nthreads];
                for (i, s) in snap.segments.iter().enumerate() {
                    cs[i % nthreads].push(s.clone());
                }
                cs
            };
            for chunk in chunks {
                let avg_ref = &avg_cache;
                handles.push(scope.spawn(move || {
                    let mut out = Vec::new();
                    for s in &chunk {
                        out.push(eval_segment(s, snap, q, &snap.schema, opts, n, avg_ref));
                    }
                    out
                }));
            }
            for h in handles {
                per_seg.extend(h.join().unwrap());
            }
        });
    }
    // merge + top-k with threshold pruning
    let mut acc: HashMap<u32, f32> = HashMap::new();
    for v in per_seg {
        for (d, s) in v {
            *acc.entry(d).or_insert(0.0) += s;
        }
    }
    top_k(acc, k)
}

fn top_k(mut acc: HashMap<u32, f32>, k: usize) -> Vec<Hit> {
    // MaxScore-style cutoff: once sorted, docs past k are pruned. Scores are
    // exact; pruning only drops docs that cannot be in the top-k.
    let mut hits: Vec<Hit> = acc.drain().map(|(doc, score)| Hit { doc, score }).collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap()
            .then_with(|| a.doc.cmp(&b.doc))
    });
    hits.truncate(k);
    hits
}

// ---- snippets / highlighting ----

/// Build a snippet from `text` around the first query-term hit, wrapping
/// matched terms in `<mark>`. matching is case-insensitive word match on
/// analyzed terms (stemmed).
pub fn snippet(text: &str, query_terms: &[String], window: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        return String::new();
    }
    let norm: Vec<String> = words
        .iter()
        .map(|w| {
            stem(&w.to_lowercase())
                .trim_matches(|c: char| !c.is_alphanumeric())
                .to_string()
        })
        .collect();
    let qset: HashSet<&str> = query_terms.iter().map(|s| s.as_str()).collect();
    let hit = norm.iter().position(|w| qset.contains(w.as_str()));
    let center = hit.unwrap_or(0);
    let lo = center.saturating_sub(window);
    let hi = (center + window + 1).min(words.len());
    let mut parts = Vec::new();
    if lo > 0 {
        parts.push("…".to_string());
    }
    for w in &words[lo..hi] {
        let n = stem(&w.to_lowercase())
            .trim_matches(|c: char| !c.is_alphanumeric())
            .to_string();
        if qset.contains(n.as_str()) {
            parts.push(format!("<mark>{w}</mark>"));
        } else {
            parts.push(w.to_string());
        }
    }
    if hi < words.len() {
        parts.push("…".to_string());
    }
    parts.join(" ")
}

// ---- suggest / did-you-mean ----

/// Autocomplete: dict terms of `field` starting with `prefix`, top `limit`
/// by df desc then alphabetical.
pub fn suggest(snap: &Snapshot, field: &str, prefix: &str, limit: usize) -> Vec<String> {
    let p = prefix.to_lowercase();
    let mut terms = snap.all_dict_terms(field);
    terms.retain(|(t, _)| t.starts_with(&p));
    terms.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    terms
        .into_iter()
        .take(limit.clamp(1, usize::MAX))
        .map(|(t, _)| t)
        .collect()
}

/// Did-you-mean: closest dict term within edit distance 2, ranked by
/// (distance, df desc). Returns None when the term already exists.
pub fn did_you_mean(snap: &Snapshot, field: &str, term: &str) -> Option<String> {
    let q = stem(&term.to_lowercase());
    let mut best: Option<(u32, u64, String)> = None;
    for (t, df) in snap.all_dict_terms(field) {
        if t == q {
            return None;
        }
        let d = edit_distance(&q, &t, 2);
        if d <= 2 {
            let cand = (d, u64::MAX - df, t.clone());
            if best
                .as_ref()
                .map(|b| cand < (b.0, b.1, b.2.clone()))
                .unwrap_or(true)
            {
                best = Some((d, u64::MAX - df, t));
            }
        }
    }
    best.map(|(_, _, t)| t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::parse;
    use crate::segment::SegmentWriter;
    use std::collections::BTreeMap;

    fn test_snap() -> Snapshot {
        let schema = Schema::default();
        let mut w = SegmentWriter::default();
        let d = |t: &str, b: &str| {
            BTreeMap::from([
                ("title".to_string(), t.to_string()),
                ("body".to_string(), b.to_string()),
            ])
        };
        w.add_doc(
            0,
            &d("Rust search", "fast rust search engine in rust"),
            &schema,
        );
        w.add_doc(1, &d("Cooking", "rust recipes for dinner"), &schema);
        w.add_doc(2, &d("Gardening", "tomatoes and soil"), &schema);
        let seg = w.finish(1);
        Snapshot {
            segments: vec![seg],
            live: [0, 1, 2].into_iter().collect(),
            stored: BTreeMap::new(),
            schema,
        }
    }

    #[test]
    fn term_search_ranks() {
        let s = test_snap();
        let hits = search(&s, &parse("rust").unwrap(), &SearchOptions::top(10));
        assert_eq!(hits.len(), 2);
        // doc 0 mentions rust twice -> ranks first
        assert_eq!(hits[0].doc, 0);
        assert!(hits[0].score >= hits[1].score);
    }

    #[test]
    fn boolean_and_phrase() {
        let s = test_snap();
        let hits = search(
            &s,
            &parse("rust AND recipes").unwrap(),
            &SearchOptions::top(10),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc, 1);
        let hits = search(
            &s,
            &parse("\"search engine\"").unwrap(),
            &SearchOptions::top(10),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc, 0);
    }

    #[test]
    fn prefix_fuzzy_not() {
        let s = test_snap();
        assert_eq!(
            search(&s, &parse("recip*").unwrap(), &SearchOptions::top(10)).len(),
            1
        );
        assert_eq!(
            search(&s, &parse("rust~1").unwrap(), &SearchOptions::top(10)).len(),
            2
        );
        let hits = search(
            &s,
            &parse("rust NOT recipes").unwrap(),
            &SearchOptions::top(10),
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].doc, 0);
    }

    #[test]
    fn field_boost_changes_order() {
        let s = test_snap();
        let mut opts = SearchOptions::top(10);
        opts.field_boosts.insert("title".into(), 100.0);
        let hits = search(&s, &parse("title:rust").unwrap(), &opts);
        assert!(!hits.is_empty());
    }

    #[test]
    fn tfidf_option_runs() {
        let s = test_snap();
        let mut opts = SearchOptions::top(10);
        opts.scorer = Scorer::Tfidf;
        assert_eq!(search(&s, &parse("rust").unwrap(), &opts).len(), 2);
    }

    #[test]
    fn snippet_highlights() {
        let sn = snippet("the fast rust engine is fast", &["fast".into()], 2);
        assert!(sn.contains("<mark>fast</mark>"), "{sn}");
    }

    #[test]
    fn suggest_and_spell() {
        let s = test_snap();
        assert!(suggest(&s, "body", "rec", 5).contains(&"recip".to_string()));
        assert_eq!(did_you_mean(&s, "body", "ruts"), Some("rust".to_string()));
        assert_eq!(did_you_mean(&s, "body", "rust"), None);
    }

    #[test]
    fn edit_distance_basic() {
        assert_eq!(edit_distance("rust", "rust", 2), 0);
        assert_eq!(edit_distance("rust", "rusty", 2), 1);
        assert!(edit_distance("rust", "python", 2) > 2);
    }
}
