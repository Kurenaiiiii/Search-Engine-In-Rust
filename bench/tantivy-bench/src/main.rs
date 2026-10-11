//! Tantivy comparison benchmark (separate crate, NOT a workspace member).
//! Indexes the same NDJSON corpus and runs the same frozen query set.
//! Usage: tantivy-bench <corpus.ndjson> <queries.txt> [--iters N]

use std::time::Instant;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{doc, Index};

fn pct(mut v: Vec<u128>, p: f64) -> u128 {
    v.sort_unstable();
    v[((p * v.len() as f64) as usize).min(v.len() - 1)]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let corpus = args.get(1).cloned().unwrap_or("../corpus.ndjson".to_string());
    let qfile = args.get(2).cloned().unwrap_or("../../tools/bench_queries.txt".to_string());
    let iters: usize = args
        .windows(2)
        .find(|w| w[0] == "--iters")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(20);

    let mut schema_builder = Schema::builder();
    let title = schema_builder.add_text_field("title", TEXT | STORED);
    let body = schema_builder.add_text_field("body", TEXT | STORED);
    let schema = schema_builder.build();
    let index = Index::create_in_ram(schema.clone());
    let mut writer = index.writer(200_000_000).unwrap();
    let t0 = Instant::now();
    let mut n = 0usize;
    for line in std::fs::read_to_string(&corpus).unwrap().lines() {
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        writer
            .add_document(doc!(
                title => v.get("title").and_then(|x| x.as_str()).unwrap_or(""),
                body => v.get("body").and_then(|x| x.as_str()).unwrap_or(""),
            ))
            .unwrap();
        n += 1;
    }
    writer.commit().unwrap();
    let index_s = t0.elapsed().as_secs_f64();
    let reader = index.reader().unwrap();
    let searcher = reader.searcher();
    let qp = QueryParser::for_index(&index, vec![title, body]);

    let queries: Vec<String> = std::fs::read_to_string(&qfile)
        .unwrap()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    print!("{{\"engine\":\"tantivy\",\"docs\":{n},\"index_s\":{index_s:.2},\"queries\":[");
    for (i, q) in queries.iter().enumerate() {
        if i > 0 {
            print!(",");
        }
        // tantivy needs explicit OR for multi-word; bare "a b" is AND there too.
        // translate our title: queries (same syntax) and trailing ~N (same).
        let tq = q.clone();
        let parsed = qp.parse_query(&tq);
        let (hits, samples) = match parsed {
            Ok(query) => {
                for _ in 0..3 {
                    let _ = searcher.search(&query, &TopDocs::with_limit(10));
                }
                let mut s = Vec::new();
                let mut h = 0;
                for _ in 0..iters {
                    let t = Instant::now();
                    h = searcher.search(&query, &TopDocs::with_limit(10)).unwrap().len();
                    s.push(t.elapsed().as_micros());
                }
                (h as i64, s)
            }
            Err(e) => {
                eprintln!("query {q:?} parse error: {e}");
                (-1, vec![0])
            }
        };
        let esc = q.replace('\\', "\\\\").replace('"', "\\\"");
        print!(
            "{{\"q\":\"{esc}\",\"hits\":{hits},\"min_us\":{},\"p50_us\":{},\"p95_us\":{},\"p99_us\":{}}}",
            samples.iter().min().copied().unwrap_or(0),
            pct(samples.clone(), 0.50),
            pct(samples.clone(), 0.95),
            pct(samples, 0.99),
        );
    }
    println!("]}}");
    eprintln!("indexed {n} docs in {index_s:.1}s");
}
