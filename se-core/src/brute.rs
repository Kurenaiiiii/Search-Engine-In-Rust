//! Naive brute-force reference engine: scans every document with no index.
//! Used by property tests as the oracle for the real engine. Scoring
//! formulas are shared with `search` so sets match exactly and scores match
//! within float tolerance.

use std::collections::{BTreeMap, HashSet};

use crate::analysis::Schema;
use crate::query::Query;
use crate::search::{bm25, edit_distance, tfidf, Hit, Scorer, SearchOptions};

pub struct Brute {
    pub docs: BTreeMap<u32, BTreeMap<String, String>>,
    pub schema: Schema,
    next_id: u32,
}

impl Brute {
    pub fn new(schema: Schema) -> Self {
        Self {
            docs: BTreeMap::new(),
            schema,
            next_id: 0,
        }
    }
    pub fn add(&mut self, fields: BTreeMap<String, String>) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.docs.insert(id, fields);
        id
    }
    pub fn delete(&mut self, id: u32) {
        self.docs.remove(&id);
    }
    /// Indexed terms of one field value (same analysis as the real engine).
    fn terms(&self, field: &str, text: &str) -> Vec<String> {
        crate::analysis::analyze(text, self.schema.analyzer_for(field))
    }
    fn terms_pos(&self, field: &str, text: &str) -> Vec<(String, u32)> {
        crate::analysis::analyze_with_positions(text, self.schema.analyzer_for(field))
    }
    fn fields_of(&self, field: &Option<String>, opts: &SearchOptions) -> Vec<String> {
        match field {
            Some(f) => vec![f.clone()],
            None if !opts.default_fields.is_empty() => opts.default_fields.clone(),
            None => {
                let mut all = HashSet::new();
                for d in self.docs.values() {
                    for k in d.keys() {
                        all.insert(k.clone());
                    }
                }
                let mut v: Vec<String> = all.into_iter().collect();
                v.sort();
                v
            }
        }
    }
    fn df(&self, field: &str, term: &str) -> u64 {
        self.docs
            .values()
            .filter(|d| {
                d.get(field)
                    .map(|t| self.terms(field, t).contains(&term.to_string()))
                    .unwrap_or(false)
            })
            .count() as u64
    }
    fn avg_len(&self, field: &str) -> f64 {
        if self.docs.is_empty() {
            return 0.0;
        }
        let sum: usize = self
            .docs
            .values()
            .map(|d| {
                d.get(field)
                    .map(|t| self.terms(field, t).len())
                    .unwrap_or(0)
            })
            .sum();
        sum as f64 / self.docs.len() as f64
    }
    fn doc_len(&self, doc: &BTreeMap<String, String>, field: &str) -> f32 {
        doc.get(field)
            .map(|t| self.terms(field, t).len())
            .unwrap_or(0) as f32
    }
    fn weight(&self, tf: f32, dl: f32, avg: f64, df: u64, n: u64, scorer: &Scorer) -> f32 {
        match scorer {
            Scorer::Bm25 { k1, b } => bm25(tf, dl, avg as f32, df, n, *k1, *b),
            Scorer::Tfidf => tfidf(tf, df, n),
        }
    }
    fn boost(&self, opts: &SearchOptions, field: &str) -> f32 {
        opts.field_boosts
            .get(field)
            .copied()
            .unwrap_or_else(|| self.schema.boost_for(field))
    }

    fn eval(
        &self,
        doc: &BTreeMap<String, String>,
        q: &Query,
        opts: &SearchOptions,
        n: u64,
    ) -> Option<f32> {
        match q {
            Query::MatchAll => Some(1.0),
            Query::Not(inner) => {
                if self.eval(doc, inner, opts, n).is_some() {
                    None
                } else {
                    Some(1.0)
                }
            }
            Query::And(parts) => {
                let mut s = 0.0;
                for p in parts {
                    s += self.eval(doc, p, opts, n)?;
                }
                Some(s)
            }
            Query::Or(parts) => {
                let mut s = 0.0;
                let mut any = false;
                for p in parts {
                    if let Some(v) = self.eval(doc, p, opts, n) {
                        s += v;
                        any = true;
                    }
                }
                if any {
                    Some(s)
                } else {
                    None
                }
            }
            Query::Term { field, term, boost } => {
                let mut s = 0.0;
                let mut any = false;
                for f in self.fields_of(field, opts) {
                    let ana = crate::analysis::analyze(term, self.schema.analyzer_for(&f));
                    let Some(qt) = ana.into_iter().next() else {
                        continue;
                    };
                    let toks = doc.get(&f).map(|t| self.terms(&f, t)).unwrap_or_default();
                    let tf = toks.iter().filter(|t| **t == qt).count() as f32;
                    if tf > 0.0 {
                        s += self.weight(
                            tf,
                            self.doc_len(doc, &f),
                            self.avg_len(&f),
                            self.df(&f, &qt).max(1),
                            n,
                            &opts.scorer,
                        ) * boost
                            * self.boost(opts, &f);
                        any = true;
                    }
                }
                if any {
                    Some(s)
                } else {
                    None
                }
            }
            Query::Prefix {
                field,
                prefix,
                boost,
            } => {
                let mut s = 0.0;
                let mut any = false;
                for f in self.fields_of(field, opts) {
                    let p = prefix.to_lowercase();
                    let toks = doc.get(&f).map(|t| self.terms(&f, t)).unwrap_or_default();
                    let mut uniq: Vec<String> =
                        toks.into_iter().filter(|t| t.starts_with(&p)).collect();
                    uniq.sort();
                    uniq.dedup();
                    for qt in uniq {
                        let tf = doc
                            .get(&f)
                            .map(|t| self.terms(&f, t).iter().filter(|t| **t == qt).count() as f32)
                            .unwrap_or(0.0);
                        s += self.weight(
                            tf,
                            self.doc_len(doc, &f),
                            self.avg_len(&f),
                            self.df(&f, &qt).max(1),
                            n,
                            &opts.scorer,
                        ) * boost
                            * self.boost(opts, &f);
                        any = true;
                    }
                }
                if any {
                    Some(s)
                } else {
                    None
                }
            }
            Query::Fuzzy {
                field,
                term,
                dist,
                boost,
            } => {
                let mut s = 0.0;
                let mut any = false;
                for f in self.fields_of(field, opts) {
                    let cfg = self.schema.analyzer_for(&f);
                    let q = if cfg.stem {
                        crate::analysis::stem(&term.to_lowercase())
                    } else {
                        term.to_lowercase()
                    };
                    // candidate dict = all corpus terms of field
                    let mut dict = HashSet::new();
                    for d in self.docs.values() {
                        if let Some(t) = d.get(&f) {
                            for t in self.terms(&f, t) {
                                dict.insert(t);
                            }
                        }
                    }
                    let mut cands: Vec<String> = dict
                        .into_iter()
                        .filter(|t| edit_distance(&q, t, *dist) <= *dist)
                        .collect();
                    cands.sort();
                    for qt in cands {
                        let tf = doc
                            .get(&f)
                            .map(|t| self.terms(&f, t).iter().filter(|t| **t == qt).count() as f32)
                            .unwrap_or(0.0);
                        if tf > 0.0 {
                            s += self.weight(
                                tf,
                                self.doc_len(doc, &f),
                                self.avg_len(&f),
                                self.df(&f, &qt).max(1),
                                n,
                                &opts.scorer,
                            ) * boost
                                * self.boost(opts, &f);
                            any = true;
                        }
                    }
                }
                if any {
                    Some(s)
                } else {
                    None
                }
            }
            Query::Phrase {
                field,
                terms,
                boost,
            } => {
                let mut s = 0.0;
                let mut any = false;
                for f in self.fields_of(field, opts) {
                    let toks = doc
                        .get(&f)
                        .map(|t| self.terms_pos(&f, t))
                        .unwrap_or_default();
                    // df = docs containing the phrase
                    let mut dfp = 0u64;
                    for d in self.docs.values() {
                        if let Some(t) = d.get(&f) {
                            let seq: Vec<String> = self.terms(&f, t);
                            if contains_phrase(&seq, terms) {
                                dfp += 1;
                            }
                        }
                    }
                    let occ = count_phrase(
                        &toks.iter().map(|(t, _)| t.clone()).collect::<Vec<_>>(),
                        terms,
                    );
                    if occ > 0 {
                        s += self.weight(
                            occ as f32,
                            self.doc_len(doc, &f),
                            self.avg_len(&f),
                            dfp.max(1),
                            n,
                            &opts.scorer,
                        ) * boost
                            * self.boost(opts, &f);
                        any = true;
                    }
                }
                if any {
                    Some(s)
                } else {
                    None
                }
            }
        }
    }

    pub fn search(&self, q: &Query, opts: &SearchOptions) -> Vec<Hit> {
        let n = (self.docs.len() as u64).max(1);
        let mut hits = Vec::new();
        for (&id, doc) in &self.docs {
            if let Some(score) = self.eval(doc, q, opts, n) {
                hits.push(Hit { doc: id, score });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap()
                .then_with(|| a.doc.cmp(&b.doc))
        });
        hits.truncate(opts.top_k.max(1));
        hits
    }
}

fn contains_phrase(seq: &[String], phrase: &[String]) -> bool {
    if phrase.is_empty() || seq.len() < phrase.len() {
        return false;
    }
    (0..=seq.len() - phrase.len()).any(|i| seq[i..i + phrase.len()] == *phrase)
}

fn count_phrase(seq: &[String], phrase: &[String]) -> usize {
    if phrase.is_empty() || seq.len() < phrase.len() {
        return 0;
    }
    (0..=seq.len() - phrase.len())
        .filter(|&i| seq[i..i + phrase.len()] == *phrase)
        .count()
}

/// Deterministic xorshift RNG for property tests (no dev-dependencies).
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % (n as u64).max(1)) as usize
    }
    pub fn pick<'a>(&mut self, xs: &'a [&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn brute_runs() {
        let schema = Schema::default();
        let mut b = Brute::new(schema);
        b.add(BTreeMap::from([("body".into(), "hello world".into())]));
        let q = crate::query::parse("hello").unwrap();
        assert_eq!(b.search(&q, &SearchOptions::top(5)).len(), 1);
    }
}
