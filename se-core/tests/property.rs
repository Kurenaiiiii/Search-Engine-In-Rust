//! Property tests: real engine vs brute-force oracle on random corpora and
//! random queries. Matching sets must be identical; scores within tolerance.

use se_core::brute::{Brute, Rng};
use se_core::index::Index;
use se_core::query::parse;
use se_core::search::SearchOptions;
use std::collections::BTreeMap;

const VOCAB: &[&str] = &[
    "rust",
    "search",
    "engine",
    "fast",
    "index",
    "query",
    "document",
    "token",
    "merge",
    "segment",
    "ranking",
    "bm25",
    "phrase",
    "fuzzy",
    "prefix",
    "wal",
    "snapshot",
    "tombstone",
];

fn random_doc(rng: &mut Rng) -> BTreeMap<String, String> {
    let n = 2 + rng.below(6);
    let mut words = Vec::new();
    for _ in 0..n {
        words.push(rng.pick(VOCAB).to_string());
    }
    let title = words[..words.len().min(3)].join(" ");
    BTreeMap::from([
        ("title".to_string(), title),
        ("body".to_string(), words.join(" ")),
    ])
}

fn random_query(rng: &mut Rng) -> String {
    let w1 = rng.pick(VOCAB).to_string();
    let w2 = rng.pick(VOCAB).to_string();
    match rng.below(9) {
        0 => w1,
        1 => format!("{w1} AND {w2}"),
        2 => format!("{w1} OR {w2}"),
        3 => format!("{w1} NOT {w2}"),
        4 => format!("\"{w1} {w2}\""),
        5 => format!("{}*", &w1[..w1.len().min(3)]),
        6 => format!("{w1}~1"),
        7 => format!("{w1} {w2}"),
        _ => format!("title:{w1}"),
    }
}

fn run_case(seed: u64, flush_each: bool, merge: bool, scorer_bm25: bool) {
    let mut rng = Rng(seed);
    let dir = std::env::temp_dir().join(format!("se_prop_{seed}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let idx = Index::open(&dir).unwrap();
    let mut brute = Brute::new(se_core::Schema::default());
    let ndocs = 5 + rng.below(20);
    for _ in 0..ndocs {
        let d = random_doc(&mut rng);
        brute.add(d.clone());
        idx.add(d).unwrap();
        if flush_each {
            idx.flush().unwrap();
        }
    }
    idx.flush().unwrap();
    if merge {
        idx.merge_all().unwrap();
    }
    for _ in 0..30 {
        let qs = random_query(&mut rng);
        let Ok(q) = parse(&qs) else { continue };
        let mut opts = SearchOptions::top(10);
        if !scorer_bm25 {
            opts.scorer = se_core::Scorer::Tfidf;
        }
        let got = idx.search(&qs, &opts).unwrap();
        let want = brute.search(&q, &opts);
        let gs: Vec<u32> = got.iter().map(|h| h.doc).collect();
        let ws: Vec<u32> = want.iter().map(|h| h.doc).collect();
        assert_eq!(gs, ws, "seed {seed} query '{qs}': set/order mismatch");
        for (g, w) in got.iter().zip(want.iter()) {
            let tol = 1e-3 * w.score.abs().max(1.0);
            assert!(
                (g.score - w.score).abs() <= tol,
                "seed {seed} query '{qs}' doc {}: {g:?} vs {w:?}",
                g.doc
            );
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn oracle_single_segment_bm25() {
    for seed in 1..=8 {
        run_case(seed, false, false, true);
    }
}

#[test]
fn oracle_multi_segment_bm25() {
    for seed in 100..=106 {
        run_case(seed, true, false, true);
    }
}

#[test]
fn oracle_merged_tfidf() {
    for seed in 200..=205 {
        run_case(seed, true, true, false);
    }
}

#[test]
fn oracle_phrase_prefix_fuzzy() {
    let mut rng = Rng(999);
    let dir = std::env::temp_dir().join(format!("se_prop_pf_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let idx = Index::open(&dir).unwrap();
    let mut brute = Brute::new(se_core::Schema::default());
    for _ in 0..15 {
        let d = random_doc(&mut rng);
        brute.add(d.clone());
        idx.add(d).unwrap();
    }
    idx.flush().unwrap();
    for qs in [
        "\"rust search\"",
        "\"fast index query\"",
        "eng*",
        "segm*",
        "rust~1",
        "bm25~2",
        "title:search",
        "(rust OR engine) AND index",
        "NOT rust",
        // free-text default OR + minimum_should_match variants
        "rust engine",
        "rust search engine",
        "rust^2 engine",
    ] {
        let q = parse(qs).unwrap();
        for msm in [1, 2] {
            let mut opts = SearchOptions::top(10);
            opts.minimum_should_match = msm;
            let got = idx.search(qs, &opts).unwrap();
            let want = brute.search(&q, &opts);
            let gs: Vec<u32> = got.iter().map(|h| h.doc).collect();
            let ws: Vec<u32> = want.iter().map(|h| h.doc).collect();
            assert_eq!(gs, ws, "query '{qs}' msm={msm}");
            for (g, w) in got.iter().zip(want.iter()) {
                let tol = 1e-3 * w.score.abs().max(1.0);
                assert!(
                    (g.score - w.score).abs() <= tol,
                    "query '{qs}' msm={msm} doc {}: {g:?} vs {w:?}",
                    g.doc
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
