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

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub top_k: usize,
    pub scorer: Scorer,
    pub field_boosts: HashMap<String, f32>,
    pub default_fields: Vec<String>,
    /// max search threads across segments; 1 = single-threaded
    pub max_threads: usize,
    /// minimum matching parts for free-text Should queries (default 1 =
    /// plain OR). Explicit OR/AND/NOT always keep strict semantics.
    pub minimum_should_match: usize,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            top_k: 10,
            scorer: Scorer::default(),
            field_boosts: HashMap::new(),
            default_fields: Vec::new(),
            max_threads: 8,
            minimum_should_match: 1,
        }
    }
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
    let mut prev = Vec::new();
    let mut cur = Vec::new();
    edit_distance_pre(&a, b, cap, &mut prev, &mut cur)
}

/// Same as [`edit_distance`] but reuses caller-provided row buffers and a
/// pre-collected query, so fuzzy expansion over thousands of candidates
/// allocates once instead of per term.
pub fn edit_distance_pre(
    a: &[char],
    b: &str,
    cap: u32,
    prev: &mut Vec<u32>,
    cur: &mut Vec<u32>,
) -> u32 {
    let bc: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return (bc.len() as u32).min(cap + 1);
    }
    if bc.is_empty() {
        return (a.len() as u32).min(cap + 1);
    }
    if a.len().abs_diff(bc.len()) as u32 > cap {
        return cap + 1;
    }
    prev.clear();
    prev.extend(0..=bc.len() as u32);
    cur.clear();
    cur.resize(bc.len() + 1, 0);
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i as u32 + 1;
        let mut row_min = cur[0];
        for (j, &cb) in bc.iter().enumerate() {
            cur[j + 1] = (prev[j] + u32::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
            row_min = row_min.min(cur[j + 1]);
        }
        if row_min > cap {
            return cap + 1;
        }
        std::mem::swap(prev, cur);
    }
    prev[bc.len()]
}

/// A snapshot of segments searched as one consistent view.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub segments: Vec<std::sync::Arc<Segment>>,
    /// live (non-deleted) global doc ids
    pub live: HashSet<u32>,
    pub stored: std::sync::Arc<BTreeMap<u32, BTreeMap<String, String>>>,
    pub schema: Schema,
    /// cached field list (avoids scanning stored docs per query)
    pub fields: Vec<String>,
    /// total docs across segments + buffer; when it equals live.len(),
    /// there are no deletes and per-posting live checks are skipped.
    pub total_docs: u64,
    pub live_complete: bool,
}

impl Snapshot {
    pub fn live_count(&self) -> u64 {
        self.live.len() as u64
    }
    pub fn all_fields(&self) -> Vec<String> {
        if self.fields.is_empty() {
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
        } else {
            self.fields.clone()
        }
    }
    /// df summed across segments, restricted to live docs. With no deletes
    /// this is just the sum of per-segment posting lengths, O(segments).
    pub fn df(&self, field: &str, term: &str) -> u64 {
        if self.live_complete {
            return self.segments.iter().map(|s| s.df(field, term) as u64).sum();
        }
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
    /// Average field length over live docs. With no deletes this sums the
    /// per-segment totals cached at flush/merge/load time.
    pub fn avg_field_len(&self, field: &str) -> f64 {
        let n = self.live_count();
        if n == 0 {
            return 0.0;
        }
        if self.live_complete {
            let sum: u64 = self
                .segments
                .iter()
                .filter_map(|s| s.field_totals.get(field))
                .map(|(t, _)| *t)
                .sum();
            return sum as f64 / n as f64;
        }
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
        if self.live_complete {
            // single ordered pass per segment; df summed across segments.
            // (no deletes here, so every list has df >= 1)
            let mut df: HashMap<String, u64> = HashMap::new();
            for s in &self.segments {
                for (t, d) in s.field_terms_with_df(field) {
                    *df.entry(t).or_insert(0) += d;
                }
            }
            let mut v: Vec<(String, u64)> = df.into_iter().collect();
            v.sort();
            return v;
        }
        let mut df: HashMap<String, u64> = HashMap::new();
        for s in &self.segments {
            for t in s.all_field_terms(field) {
                let d = self.df(field, &t);
                if d > 0 {
                    *df.entry(t).or_insert(0) += d;
                }
            }
        }
        let mut v: Vec<(String, u64)> = df.into_iter().collect();
        v.sort();
        v
    }
    /// Dict terms of a field whose char length is in [lo, hi], with df.
    /// Only length-compatible buckets are visited — fuzzy expansion never
    /// scans the whole dictionary.
    pub fn dict_terms_in_len_range(&self, field: &str, lo: usize, hi: usize) -> Vec<(String, u64)> {
        let mut df: HashMap<String, u64> = HashMap::new();
        for s in &self.segments {
            for t in s.terms_with_len(field, lo, hi) {
                let d = self.df(field, &t);
                if d > 0 {
                    *df.entry(t).or_insert(0) += d;
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
                // length buckets first: a match must satisfy
                // |len(t) - len(q)| <= dist (necessary condition), so only
                // compatible buckets are visited instead of the whole dict.
                // The bucket range makes a recount gate redundant; row
                // buffers and the query chars are reused across candidates.
                let qc: Vec<char> = q.chars().collect();
                let lo = qc.len().saturating_sub(dist as usize);
                let hi = qc.len() + dist as usize;
                let mut prev = Vec::new();
                let mut cur = Vec::new();
                for s in &snap.segments {
                    for t in s.terms_with_len(&f, lo, hi) {
                        if edit_distance_pre(&qc, &t, dist, &mut prev, &mut cur) <= dist {
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

/// idf part of BM25, hoisted out of per-posting loops (identical result,
/// one `ln()` per clause instead of one per posting).
fn bm25_idf(df: u64, n: u64) -> f32 {
    (((n as f64 - df as f64 + 0.5) / (df as f64 + 0.5)) + 1.0).ln() as f32
}

fn tfidf_idf(df: u64, n: u64) -> f32 {
    (1.0 + n as f32 / (df as f32 + 1.0)).ln()
}

fn bm25_with_idf(tf: f32, doc_len: f32, avg_len: f32, idf: f32, k1: f32, b: f32) -> f32 {
    let norm = 1.0 - b + b * (doc_len / avg_len.max(1e-6));
    idf * (tf * (k1 + 1.0)) / (tf + k1 * norm)
}

fn tfidf_with_idf(tf: f32, idf: f32) -> f32 {
    (1.0 + tf.ln_1p()) * idf
}

/// Dense per-segment score accumulator. Local doc ids are dense
/// (0..doc_count), so scoring writes into a Vec instead of hashing every
/// posting; only touched docs are visited afterwards.
struct DenseAcc {
    scores: Vec<f32>,
    seen: Vec<bool>,
    touched: Vec<u32>,
}

impl DenseAcc {
    fn new(n: usize) -> Self {
        Self {
            scores: vec![0.0; n],
            seen: vec![false; n],
            touched: Vec::new(),
        }
    }
    fn add(&mut self, local: u32, s: f32) {
        let i = local as usize;
        if !self.seen[i] {
            self.seen[i] = true;
            self.touched.push(local);
        }
        self.scores[i] += s;
    }
}

#[derive(Debug, Clone)]
struct Clause {
    field: String,
    term: String,
    boost: f32,
    max_score: f32,
}

/// Precomputed per-search collection statistics. Expansion, df and phrase-df
/// depend only on the snapshot, so they are computed once per search instead
/// of once per segment (which was quadratic in the segment count).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ExpKey {
    field: Option<String>,
    kind: u8, // 0 = term, 1 = prefix, 2 = fuzzy
    raw: String,
    dist: u32,
}

#[derive(Debug, Clone, Default)]
pub struct QueryStats {
    avg: HashMap<String, f64>,
    df: HashMap<(String, String), u64>,
    expansions: HashMap<ExpKey, Vec<(String, String)>>,
    pdf: HashMap<(String, String), u64>,
}

fn collect_keys(
    q: &Query,
    keys: &mut Vec<ExpKey>,
    phrases: &mut Vec<(Option<String>, Vec<String>)>,
) {
    match q {
        Query::Term { field, term, .. } => keys.push(ExpKey {
            field: field.clone(),
            kind: 0,
            raw: term.clone(),
            dist: 0,
        }),
        Query::Prefix { field, prefix, .. } => keys.push(ExpKey {
            field: field.clone(),
            kind: 1,
            raw: prefix.clone(),
            dist: 0,
        }),
        Query::Fuzzy {
            field, term, dist, ..
        } => keys.push(ExpKey {
            field: field.clone(),
            kind: 2,
            raw: term.clone(),
            dist: *dist,
        }),
        Query::Phrase { field, terms, .. } => phrases.push((field.clone(), terms.clone())),
        Query::And(v) | Query::Or(v) | Query::Should(v) => {
            for p in v {
                collect_keys(p, keys, phrases);
            }
        }
        Query::Not(inner) => collect_keys(inner, keys, phrases),
        Query::MatchAll => {}
    }
}

fn expand_kind(kind: u8) -> &'static str {
    match kind {
        1 => "prefix",
        2 => "fuzzy",
        _ => "term",
    }
}

impl QueryStats {
    pub fn build(snap: &Snapshot, schema: &Schema, opts: &SearchOptions, q: &Query) -> Self {
        let mut st = QueryStats::default();
        for f in &snap.fields {
            st.avg.insert(f.clone(), snap.avg_field_len(f));
        }
        let mut keys = Vec::new();
        let mut phrases = Vec::new();
        collect_keys(q, &mut keys, &mut phrases);
        keys.sort();
        keys.dedup();
        for k in keys {
            let terms = expand_term(
                snap,
                schema,
                &k.field,
                expand_kind(k.kind),
                &k.raw,
                k.dist,
                opts,
            );
            for (f, t) in &terms {
                st.df
                    .entry((f.clone(), t.clone()))
                    .or_insert_with(|| snap.df(f, t).max(1));
            }
            st.expansions.insert(k, terms);
        }
        let mut seen = HashSet::new();
        for (field, terms) in &phrases {
            for f in resolve_fields(field, snap, opts) {
                let pk = (f.clone(), terms.join("\u{1}"));
                if seen.insert(pk.clone()) {
                    let df = phrase_df(snap, &f, terms);
                    st.pdf.insert(pk, df);
                }
            }
        }
        st
    }
    pub fn df_of(&self, field: &str, term: &str) -> u64 {
        self.df
            .get(&(field.to_string(), term.to_string()))
            .copied()
            .unwrap_or(1)
    }
    pub fn avg_of(&self, field: &str) -> f64 {
        self.avg.get(field).copied().unwrap_or(0.0)
    }
    pub fn pdf_of(&self, field: &str, terms: &[String]) -> u64 {
        self.pdf
            .get(&(field.to_string(), terms.join("\u{1}")))
            .copied()
            .unwrap_or(0)
    }
}

/// Shared evaluation context: everything except the query node and the
/// root flag. Passed by reference through the recursive evaluator.
#[derive(Clone, Copy)]
struct EvalCtx<'a> {
    seg: &'a Segment,
    snap: &'a Snapshot,
    schema: &'a Schema,
    opts: &'a SearchOptions,
    n: u64,
    st: &'a QueryStats,
}

/// Evaluate a query on one segment; returns (global_doc, score) pairs.
/// `top` is true only for the root call: only there may MaxScore truncate
/// to top-k, because And/Not parents need the full matching set.
fn eval_segment(ctx: &EvalCtx, q: &Query, top: bool) -> Vec<(u32, f32)> {
    let seg = ctx.seg;
    let snap = ctx.snap;
    let schema = ctx.schema;
    let opts = ctx.opts;
    let n = ctx.n;
    let st = ctx.st;
    match q {
        Query::MatchAll => seg
            .docs
            .iter()
            .filter(|g| snap.live.contains(g))
            .map(|&g| (g, 1.0))
            .collect(),
        Query::Not(inner) => {
            let bad: HashSet<u32> = eval_segment(ctx, inner, false)
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
                let m: HashMap<u32, f32> = eval_segment(ctx, p, false).into_iter().collect();
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
            // Flat disjunctions of plain terms use MaxScore WAND pruning,
            // but only at the query root: nested parents need full sets.
            // Anything nested or non-flat falls back to hash accumulation.
            if top {
                if let Some(hits) = maxscore_or(ctx, parts) {
                    return hits;
                }
            }
            let mut acc: HashMap<u32, f32> = HashMap::new();
            for p in parts {
                for (d, s) in eval_segment(ctx, p, false) {
                    *acc.entry(d).or_insert(0.0) += s;
                }
            }
            acc.into_iter().collect()
        }
        Query::Should(parts) => {
            let msm = opts.minimum_should_match.max(1);
            if msm <= 1 {
                // plain OR: identical scores via the Or path (incl. WAND)
                return eval_segment(ctx, &Query::Or(parts.clone()), top);
            }
            // docs must match at least msm parts; scores sum like Or.
            // counts are per part (a multi-term prefix/fuzzy part counts
            // once), matching the brute-force oracle exactly.
            let mut acc: HashMap<u32, (f32, usize)> = HashMap::new();
            for p in parts {
                for (d, s) in eval_segment(ctx, p, false) {
                    let e = acc.entry(d).or_insert((0.0, 0));
                    e.0 += s;
                    e.1 += 1;
                }
            }
            acc.into_iter()
                .filter(|(_, (_, c))| *c >= msm)
                .map(|(d, (s, _))| (d, s))
                .collect()
        }
        Query::Term { field, term, boost } => {
            let key = ExpKey {
                field: field.clone(),
                kind: 0,
                raw: term.clone(),
                dist: 0,
            };
            let clauses = expand_clauses(st, schema, opts, &key, *boost, n);
            score_clauses(seg, snap, schema, opts, n, st, &clauses)
        }
        Query::Prefix {
            field,
            prefix,
            boost,
        } => {
            let key = ExpKey {
                field: field.clone(),
                kind: 1,
                raw: prefix.clone(),
                dist: 0,
            };
            let clauses = expand_clauses(st, schema, opts, &key, *boost, n);
            score_clauses(seg, snap, schema, opts, n, st, &clauses)
        }
        Query::Fuzzy {
            field,
            term,
            dist,
            boost,
        } => {
            let key = ExpKey {
                field: field.clone(),
                kind: 2,
                raw: term.clone(),
                dist: *dist,
            };
            let clauses = expand_clauses(st, schema, opts, &key, *boost, n);
            score_clauses(seg, snap, schema, opts, n, st, &clauses)
        }
        Query::Phrase {
            field,
            terms,
            boost,
        } => {
            let mut acc = DenseAcc::new(seg.doc_count());
            for f in resolve_fields(field, snap, opts) {
                let df_phrase = st.pdf_of(&f, terms);
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
                let boost_f = boost * opts.field_boost(schema, &f);
                match &opts.scorer {
                    Scorer::Bm25 { k1, b } => {
                        let idf = bm25_idf(df_phrase, n);
                        let avg = st.avg_of(&f) as f32;
                        for m in intersect_lists(&lists, seg, snap) {
                            let occ = phrase_count_at(&lists, &m.idx);
                            if occ == 0 {
                                continue;
                            }
                            let dl = snap.doc_field_len(seg, m.doc, &f);
                            acc.add(
                                m.doc,
                                bm25_with_idf(occ as f32, dl, avg, idf, *k1, *b) * boost_f,
                            );
                        }
                    }
                    Scorer::Tfidf => {
                        let idf = tfidf_idf(df_phrase, n);
                        for m in intersect_lists(&lists, seg, snap) {
                            let occ = phrase_count_at(&lists, &m.idx);
                            if occ == 0 {
                                continue;
                            }
                            acc.add(m.doc, tfidf_with_idf(occ as f32, idf) * boost_f);
                        }
                    }
                }
            }
            dense_to_global(seg, snap, acc)
        }
    }
}

/// Local doc ids present in every posting list, with each list's posting
/// index attached so position checks need no searching.
struct DocMatch {
    doc: u32,
    idx: Vec<usize>,
}

/// Walks the shortest list and gallops the others (cursors only move
/// forward: amortized O(total postings)); live docs are filtered here so
/// callers never hash per posting.
fn intersect_lists(lists: &[&[Posting]], seg: &Segment, snap: &Snapshot) -> Vec<DocMatch> {
    if lists.is_empty() {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..lists.len()).collect();
    order.sort_by_key(|&i| lists[i].len());
    let (first, rest) = order.split_first().unwrap();
    let base = lists[*first];
    let others: Vec<usize> = rest.to_vec();
    let mut out = Vec::new();
    // per-list cursors only move forward: amortized O(total postings)
    let mut curs = vec![0usize; others.len()];
    for (bi, p) in base.iter().enumerate() {
        let doc = p.doc;
        let mut hit = true;
        let mut idx = vec![0usize; lists.len()];
        idx[*first] = bi;
        for (oi, &li) in others.iter().enumerate() {
            let ol = lists[li];
            let mut c = curs[oi];
            while c < ol.len() && ol[c].doc < doc {
                c += 1;
            }
            curs[oi] = c;
            if c >= ol.len() || ol[c].doc != doc {
                hit = false;
                break;
            }
            idx[li] = c;
        }
        if !hit {
            continue;
        }
        if snap.live_complete {
            out.push(DocMatch { doc, idx });
        } else if let Some(&gid) = seg.docs.get(doc as usize) {
            if snap.live.contains(&gid) {
                out.push(DocMatch { doc, idx });
            }
        }
    }
    out
}

fn expand_clauses(
    st: &QueryStats,
    schema: &Schema,
    opts: &SearchOptions,
    key: &ExpKey,
    boost: f32,
    n: u64,
) -> Vec<Clause> {
    let mut clauses = Vec::new();
    let empty = Vec::new();
    for (f, t) in st.expansions.get(key).unwrap_or(&empty) {
        let df = st.df_of(f, t);
        let avg = st.avg_of(f);
        // optimistic max: tf=10 at avg length
        let max_score = term_score(10.0, avg as f32, avg, df, n, &opts.scorer)
            * boost
            * opts.field_boost(schema, f);
        clauses.push(Clause {
            field: f.clone(),
            term: t.clone(),
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
    st: &QueryStats,
    clauses: &[Clause],
) -> Vec<(u32, f32)> {
    let mut acc = DenseAcc::new(seg.doc_count());
    let live_check = !snap.live_complete;
    for c in clauses {
        let df = st.df_of(&c.field, &c.term);
        let avg = st.avg_of(&c.field) as f32;
        let boost = c.boost * opts.field_boost(schema, &c.field);
        let Some(pl) = seg.postings_for(&c.field, &c.term) else {
            continue;
        };
        match &opts.scorer {
            Scorer::Bm25 { k1, b } => {
                let idf = bm25_idf(df, n);
                for p in pl {
                    if live_check {
                        match seg.docs.get(p.doc as usize) {
                            Some(gid) if snap.live.contains(gid) => {}
                            _ => continue,
                        }
                    }
                    let dl = snap.doc_field_len(seg, p.doc, &c.field);
                    acc.add(
                        p.doc,
                        bm25_with_idf(p.freq as f32, dl, avg, idf, *k1, *b) * boost,
                    );
                }
            }
            Scorer::Tfidf => {
                let idf = tfidf_idf(df, n);
                for p in pl {
                    if live_check {
                        match seg.docs.get(p.doc as usize) {
                            Some(gid) if snap.live.contains(gid) => {}
                            _ => continue,
                        }
                    }
                    // TF-IDF needs no length norm: skip the field_lens lookup
                    acc.add(p.doc, tfidf_with_idf(p.freq as f32, idf) * boost);
                }
            }
        }
    }
    dense_to_global(seg, snap, acc)
}

/// Drain a dense accumulator to (global doc, score), applying the live
/// filter once per touched doc instead of once per posting.
fn dense_to_global(seg: &Segment, snap: &Snapshot, acc: DenseAcc) -> Vec<(u32, f32)> {
    let mut out = Vec::with_capacity(acc.touched.len());
    if snap.live_complete {
        for local in acc.touched {
            if let Some(&gid) = seg.docs.get(local as usize) {
                out.push((gid, acc.scores[local as usize]));
            }
        }
    } else {
        for local in acc.touched {
            if let Some(&gid) = seg.docs.get(local as usize) {
                if snap.live.contains(&gid) {
                    out.push((gid, acc.scores[local as usize]));
                }
            }
        }
    }
    out
}

/// Global phrase df: live docs in any segment containing the exact phrase.
/// Same galloping intersection as eval, without scoring.
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
        for m in intersect_lists(&lists, seg, snap) {
            if phrase_count_at(&lists, &m.idx) > 0 {
                n += 1;
            }
        }
    }
    n
}

fn phrase_count_at(lists: &[&[Posting]], idx: &[usize]) -> usize {
    let first = &lists[0][idx[0]].positions;
    let rest = &lists[1..];
    let ridx = &idx[1..];
    first
        .iter()
        .filter(|&&base| {
            rest.iter().enumerate().all(|(i, pl)| {
                pl[ridx[i]]
                    .positions
                    .binary_search(&(base + i as u32 + 1))
                    .is_ok()
            })
        })
        .count()
}

/// MaxScore dynamic pruning for a flat disjunction of plain terms.
/// Clauses are visited most-promising first; each candidate doc carries an
/// optimistic bound (exact partial score + max of the rest) and docs whose
/// bound falls below the current top-k threshold are skipped without full
/// scoring. Skipping is exact: pruned docs provably cannot enter the top-k,
/// so matching sets and scores are identical to dense evaluation.
fn maxscore_or(ctx: &EvalCtx, parts: &[Query]) -> Option<Vec<(u32, f32)>> {
    let seg = ctx.seg;
    let snap = ctx.snap;
    let schema = ctx.schema;
    let opts = ctx.opts;
    let n = ctx.n;
    let st = ctx.st;
    // only flat Term disjunctions take this path
    let mut term_parts = Vec::new();
    for p in parts {
        match p {
            Query::Term { field, term, boost } => {
                term_parts.push((field.clone(), term.clone(), *boost))
            }
            _ => return None,
        }
    }
    struct Cursor<'a> {
        field: String,
        boost: f32,
        idf: f32,
        max_score: f32,
        list: &'a [Posting],
        cur: usize,
    }
    let mut clauses: Vec<Cursor> = Vec::new();
    for (field, term, boost) in &term_parts {
        let key = ExpKey {
            field: field.clone(),
            kind: 0,
            raw: term.clone(),
            dist: 0,
        };
        for c in expand_clauses(st, schema, opts, &key, *boost, n) {
            if let Some(pl) = seg.postings_for(&c.field, &c.term) {
                if pl.is_empty() {
                    continue;
                }
                let df = st.df_of(&c.field, &c.term);
                let idf = match &opts.scorer {
                    Scorer::Bm25 { .. } => bm25_idf(df, n),
                    Scorer::Tfidf => tfidf_idf(df, n),
                };
                clauses.push(Cursor {
                    field: c.field.clone(),
                    boost: c.boost * opts.field_boost(schema, &c.field),
                    idf,
                    max_score: c.max_score,
                    list: pl,
                    cur: 0,
                });
            }
        }
    }
    if clauses.is_empty() {
        return Some(Vec::new());
    }
    // most promising first (expand_clauses already sorts, flattening keeps it
    // near-sorted; re-sort to be exact)
    clauses.sort_by(|a, b| b.max_score.partial_cmp(&a.max_score).unwrap());
    let total_max: f32 = clauses.iter().map(|c| c.max_score).sum();
    let k = opts.top_k.max(1);
    // min-heap of (reversed score bits, doc): worst kept hit on top.
    // scores are always >= 0, where to_bits order matches numeric order.
    let mut heap: std::collections::BinaryHeap<(std::cmp::Reverse<u32>, u32)> =
        std::collections::BinaryHeap::new();
    let threshold = |heap: &std::collections::BinaryHeap<(std::cmp::Reverse<u32>, u32)>| -> f32 {
        if heap.len() < k {
            0.0
        } else {
            f32::from_bits(heap.peek().map(|(r, _)| r.0).unwrap_or(0))
        }
    };
    let avg_of = |field: &str| st.avg_of(field) as f32;
    loop {
        // next candidate = smallest doc under any cursor
        let mut d = u32::MAX;
        for c in &clauses {
            if c.cur < c.list.len() {
                d = d.min(c.list[c.cur].doc);
            }
        }
        if d == u32::MAX {
            break;
        }
        let t = threshold(&heap);
        let mut partial = 0.0f32;
        let mut rem = total_max;
        let mut pruned = false;
        for c in clauses.iter_mut() {
            while c.cur < c.list.len() && c.list[c.cur].doc < d {
                c.cur += 1;
            }
            if c.cur < c.list.len() && c.list[c.cur].doc == d {
                let p = &c.list[c.cur];
                let s = match &opts.scorer {
                    Scorer::Bm25 { k1, b } => {
                        let dl = snap.doc_field_len(seg, p.doc, &c.field);
                        bm25_with_idf(p.freq as f32, dl, avg_of(&c.field), c.idf, *k1, *b)
                    }
                    Scorer::Tfidf => tfidf_with_idf(p.freq as f32, c.idf),
                } * c.boost;
                partial += s;
            }
            rem -= c.max_score;
            if partial + rem < t {
                pruned = true;
                break;
            }
        }
        // advance every cursor sitting on d (pruned or scored, d is settled)
        for c in clauses.iter_mut() {
            if c.cur < c.list.len() && c.list[c.cur].doc == d {
                c.cur += 1;
            }
        }
        if pruned {
            continue;
        }
        let live_ok = match seg.docs.get(d as usize) {
            Some(&gid) => {
                if !snap.live_complete && !snap.live.contains(&gid) {
                    None
                } else {
                    Some(gid)
                }
            }
            None => None,
        };
        if let Some(gid) = live_ok {
            heap.push((std::cmp::Reverse(partial.to_bits()), gid));
            if heap.len() > k {
                heap.pop();
            }
        }
    }
    let mut out: Vec<(u32, f32)> = heap
        .into_iter()
        .map(|(r, gid)| (gid, f32::from_bits(r.0)))
        .collect();
    // restore deterministic order for downstream merging
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0)));
    Some(out)
}

/// Search a snapshot, multi-threaded across segments. Returns top-k hits
/// sorted by (score desc, doc asc).
pub fn search(snap: &Snapshot, q: &Query, opts: &SearchOptions) -> Vec<Hit> {
    let n = snap.live_count().max(1);
    let st = QueryStats::build(snap, &snap.schema, opts, q);
    let k = opts.top_k.max(1);
    // one thread per segment (cap 8), merged with a heap
    let mut per_seg: Vec<Vec<(u32, f32)>> = Vec::new();
    let nthreads = snap.segments.len().clamp(1, opts.max_threads.max(1));
    if snap.segments.len() <= 1 || nthreads == 1 {
        for s in &snap.segments {
            let ctx = EvalCtx {
                seg: s,
                snap,
                schema: &snap.schema,
                opts,
                n,
                st: &st,
            };
            per_seg.push(eval_segment(&ctx, q, true));
        }
    } else {
        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            // chunk segments across threads
            let chunks: Vec<Vec<std::sync::Arc<Segment>>> = {
                let mut cs: Vec<Vec<std::sync::Arc<Segment>>> = vec![Vec::new(); nthreads];
                for (i, s) in snap.segments.iter().enumerate() {
                    cs[i % nthreads].push(std::sync::Arc::clone(s));
                }
                cs
            };
            for chunk in chunks {
                let st_ref = &st;
                handles.push(scope.spawn(move || {
                    let mut out = Vec::new();
                    for s in &chunk {
                        let ctx = EvalCtx {
                            seg: s,
                            snap,
                            schema: &snap.schema,
                            opts,
                            n,
                            st: st_ref,
                        };
                        out.push(eval_segment(&ctx, q, true));
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
    if per_seg.len() == 1 {
        // single segment: top-k straight from its hits, no hash merge
        let hits: Vec<Hit> = per_seg
            .pop()
            .unwrap()
            .into_iter()
            .map(|(doc, score)| Hit { doc, score })
            .collect();
        return top_k_hits(hits, k);
    }
    let mut acc: HashMap<u32, f32> = HashMap::new();
    for v in per_seg {
        for (d, s) in v {
            *acc.entry(d).or_insert(0.0) += s;
        }
    }
    top_k_hits(
        acc.drain().map(|(doc, score)| Hit { doc, score }).collect(),
        k,
    )
}

fn top_k_hits(mut hits: Vec<Hit>, k: usize) -> Vec<Hit> {
    let k = k.max(1);
    // select_nth avoids a full sort; ties are impossible (docs unique), so
    // the result is deterministic after the final top-k sort.
    if hits.len() > k {
        hits.select_nth_unstable_by(k, |a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap()
                .then_with(|| a.doc.cmp(&b.doc))
        });
        hits.truncate(k);
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap()
            .then_with(|| a.doc.cmp(&b.doc))
    });
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
            segments: vec![std::sync::Arc::new(seg)],
            live: [0, 1, 2].into_iter().collect(),
            stored: std::sync::Arc::new(BTreeMap::new()),
            schema,
            fields: vec!["body".to_string(), "title".to_string()],
            total_docs: 3,
            live_complete: true,
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

    #[test]
    fn wand_or_matches_exhaustive_or() {
        // WAND vs exhaustive scoring equivalence: 5 high-score docs come
        // first so the top-3 threshold binds immediately and the remaining
        // ~115 low-score docs must be pruned without changing results.
        use std::sync::Arc;
        let schema = Schema::default();
        let mut w = SegmentWriter::default();
        for i in 0..120u32 {
            let body = if i < 5 {
                "zelkova zelkova zelkova common filler words here".to_string()
            } else {
                format!("common filler words here doc number {i}")
            };
            w.add_doc(i, &BTreeMap::from([("body".to_string(), body)]), &schema);
        }
        let seg = w.finish(1);
        let snap = Snapshot {
            segments: vec![Arc::new(seg)],
            live: (0..120u32).collect(),
            stored: Arc::new(BTreeMap::new()),
            schema: schema.clone(),
            fields: vec!["body".to_string()],
            total_docs: 120,
            live_complete: true,
        };
        let opts = SearchOptions::top(3);
        let Query::Or(parts) = parse("zelkova OR common").unwrap() else {
            panic!("expected a flat Or");
        };
        assert_eq!(parts.len(), 2);
        let st = QueryStats::build(&snap, &snap.schema, &opts, &Query::Or(parts.clone()));
        let n = snap.live_count();
        let seg = &snap.segments[0];
        let ctx = EvalCtx {
            seg,
            snap: &snap,
            schema: &snap.schema,
            opts: &opts,
            n,
            st: &st,
        };
        let wand = maxscore_or(&ctx, &parts).expect("flat terms take the WAND path");
        // exhaustive: score every live doc by summing the same clauses
        let mut clauses = Vec::new();
        for p in &parts {
            let Query::Term { field, term, boost } = p else {
                panic!("expected Term parts");
            };
            let key = ExpKey {
                field: field.clone(),
                kind: 0,
                raw: term.clone(),
                dist: 0,
            };
            clauses.extend(expand_clauses(&st, &snap.schema, &opts, &key, *boost, n));
        }
        let mut expected: Vec<(u32, f32)> = Vec::new();
        for (li, &gid) in seg.docs.iter().enumerate() {
            let mut s = 0.0f32;
            let mut any = false;
            for c in &clauses {
                let df = st.df_of(&c.field, &c.term);
                let avg = st.avg_of(&c.field) as f32;
                if let Some(pl) = seg.postings_for(&c.field, &c.term) {
                    if let Some(p) = pl.iter().find(|p| p.doc as usize == li) {
                        let dl = snap.doc_field_len(seg, p.doc, &c.field);
                        let Scorer::Bm25 { k1, b } = opts.scorer else {
                            panic!("expected default BM25");
                        };
                        s += bm25_with_idf(p.freq as f32, dl, avg, bm25_idf(df, n), k1, b)
                            * c.boost
                            * opts.field_boost(&snap.schema, &c.field);
                        any = true;
                    }
                }
            }
            if any {
                expected.push((gid, s));
            }
        }
        expected.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap().then_with(|| a.0.cmp(&b.0)));
        expected.truncate(3);
        assert_eq!(wand.len(), expected.len(), "same top-k set size");
        assert!(
            wand.iter()
                .map(|(d, _)| d)
                .eq(expected.iter().map(|(d, _)| d)),
            "WAND top-k docs differ: {wand:?} vs {expected:?}"
        );
        for ((_, ws), (_, es)) in wand.iter().zip(expected.iter()) {
            let tol = 1e-6 * es.abs().max(1.0);
            assert!((ws - es).abs() <= tol, "WAND score differs: {ws} vs {es}");
        }
        // high-score docs must win
        assert!(wand.iter().all(|(d, _)| *d < 5));
    }
}
