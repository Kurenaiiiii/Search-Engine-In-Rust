//! se-cli: index NDJSON files, run searches, show stats, serve the API.

use se_core::search::{snippet, Scorer, SearchOptions};
use se_core::Index;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn usage() -> ! {
    eprintln!(
        "usage:
  se-cli --dir <data> index <file.ndjson>     index documents (one JSON object per line)
  se-cli --dir <data> search <query> [--top-k N] [--scorer bm25|tfidf] [--json]
  se-cli --dir <data> stats
  se-cli --dir <data> optimize                  merge all segments into one
  se-cli --dir <data> suggest <prefix> [--field body]
  se-cli --dir <data> bench --queries <file> [--iters 20]
  se-cli --dir <data> serve [--addr 127.0.0.1:3000]"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut dir = String::from("./data");
    let mut rest: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--dir" && i + 1 < args.len() {
            dir = args[i + 1].clone();
            i += 2;
        } else {
            rest.push(args[i].clone());
            i += 1;
        }
    }
    if rest.is_empty() {
        usage();
    }
    match rest[0].as_str() {
        "index" => {
            let file = rest.get(1).unwrap_or_else(|| {
                eprintln!("index needs a file argument");
                std::process::exit(2);
            });
            cmd_index(&dir, file);
        }
        "search" => {
            let q = rest.get(1).unwrap_or_else(|| {
                eprintln!("search needs a query argument");
                std::process::exit(2);
            });
            let mut top_k = 10usize;
            let mut scorer = Scorer::Bm25 { k1: 1.2, b: 0.75 };
            let mut json = false;
            let mut j = 2;
            while j < rest.len() {
                match rest[j].as_str() {
                    "--top-k" => {
                        top_k = rest.get(j + 1).and_then(|v| v.parse().ok()).unwrap_or(10);
                        j += 2;
                    }
                    "--scorer" => {
                        if rest.get(j + 1).map(|v| v == "tfidf").unwrap_or(false) {
                            scorer = Scorer::Tfidf;
                        }
                        j += 2;
                    }
                    "--json" => {
                        json = true;
                        j += 1;
                    }
                    _ => j += 1,
                }
            }
            cmd_search(&dir, q, top_k, scorer, json);
        }
        "stats" => cmd_stats(&dir),
        "optimize" => {
            let idx = open(&dir);
            let t0 = std::time::Instant::now();
            let mut rounds = 0u32;
            while idx.stats().segments > 1
                && idx.merge_tier().unwrap_or_else(|e| {
                    eprintln!("merge error: {e}");
                    std::process::exit(1);
                })
            {
                rounds += 1;
            }
            println!(
                "merged in {rounds} rounds, {:.1}s",
                t0.elapsed().as_secs_f64()
            );
            cmd_stats(&dir);
        }
        "suggest" => {
            let prefix = rest.get(1).unwrap_or_else(|| {
                eprintln!("suggest needs a prefix argument");
                std::process::exit(2);
            });
            let mut field = "body".to_string();
            if rest.get(2).map(|v| v == "--field").unwrap_or(false) {
                field = rest.get(3).cloned().unwrap_or(field);
            }
            cmd_suggest(&dir, &field, prefix);
        }
        "serve" => {
            let mut addr =
                std::env::var("SE_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
            let mut k = 1;
            while k < rest.len() {
                if rest[k] == "--addr" && k + 1 < rest.len() {
                    addr = rest[k + 1].clone();
                    k += 2;
                } else {
                    k += 1;
                }
            }
            if let Err(e) = se_server::run(&addr, &PathBuf::from(&dir)) {
                eprintln!("server error: {e}");
                std::process::exit(1);
            }
        }
        "bench" => {
            let mut queries = "tools/bench_queries.txt".to_string();
            let mut iters = 20usize;
            let mut threads = 8usize;
            let mut k = 1;
            while k < rest.len() {
                match rest[k].as_str() {
                    "--queries" if k + 1 < rest.len() => {
                        queries = rest[k + 1].clone();
                        k += 2;
                    }
                    "--iters" if k + 1 < rest.len() => {
                        iters = rest[k + 1].parse().unwrap_or(20);
                        k += 2;
                    }
                    "--threads" if k + 1 < rest.len() => {
                        threads = rest[k + 1].parse().unwrap_or(8);
                        k += 2;
                    }
                    _ => k += 1,
                }
            }
            cmd_bench(&dir, &queries, iters, threads);
        }
        _ => usage(),
    }
}

fn open(dir: &str) -> Index {
    Index::open(&PathBuf::from(dir)).unwrap_or_else(|e| {
        eprintln!("cannot open index at {dir}: {e}");
        std::process::exit(1);
    })
}

fn cmd_index(dir: &str, file: &str) {
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| {
        eprintln!("cannot read {file}: {e}");
        std::process::exit(1);
    });
    let idx = open(dir);
    let t0 = std::time::Instant::now();
    let mut n = 0usize;
    for (ln, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match se_server::parse_json(line.trim()) {
            Ok(se_server::J::Obj(pairs)) => {
                let mut m = BTreeMap::new();
                for (k, v) in pairs {
                    let s = match v {
                        se_server::J::Str(s) => s,
                        se_server::J::Num(x) => x.to_string(),
                        se_server::J::Bool(b) => b.to_string(),
                        se_server::J::Null => continue,
                        _ => {
                            eprintln!("line {}: nested values not supported, skipped", ln + 1);
                            continue;
                        }
                    };
                    m.insert(k, s);
                }
                if let Err(e) = idx.add(m) {
                    eprintln!("line {}: {e}", ln + 1);
                }
                n += 1;
            }
            _ => eprintln!("line {}: invalid JSON object, skipped", ln + 1),
        }
    }
    idx.flush().unwrap();
    let dt = t0.elapsed().as_secs_f64();
    println!(
        "indexed {n} documents in {dt:.2}s ({:.0} docs/s)",
        n as f64 / dt.max(1e-9)
    );
}

fn cmd_search(dir: &str, q: &str, top_k: usize, scorer: Scorer, json: bool) {
    let idx = open(dir);
    let opts = SearchOptions {
        top_k,
        scorer,
        ..Default::default()
    };
    let t0 = std::time::Instant::now();
    let hits = idx.search(q, &opts).unwrap_or_else(|e| {
        eprintln!("query error: {e}");
        std::process::exit(1);
    });
    let ms = t0.elapsed().as_millis();
    let terms = se_core::parse(q)
        .map(|qq| qq.literal_terms())
        .unwrap_or_default();
    if json {
        print!("{{\"took_ms\":{ms},\"hits\":[");
        for (i, h) in hits.iter().enumerate() {
            if i > 0 {
                print!(",");
            }
            print!("{{\"id\":{},\"score\":{:.4}}}", h.doc, h.score);
        }
        println!("]}}");
        return;
    }
    println!("{} hits in {ms} ms for: {q}", hits.len());
    for h in &hits {
        let stored = idx.stored(h.doc).unwrap_or_default();
        let title = stored
            .get("title")
            .cloned()
            .unwrap_or_else(|| format!("doc {}", h.doc));
        println!("  #{:<6} {:8.4}  {title}", h.doc, h.score);
        if let Some(body) = stored.get("body") {
            println!("           {}", snippet(body, &terms, 10));
        }
    }
}

fn cmd_stats(dir: &str) {
    let idx = open(dir);
    let s = idx.stats();
    println!("live_docs:     {}", s.live_docs);
    println!("segments:      {}", s.segments);
    println!("buffered_docs: {}", s.buffered_docs);
    println!("postings:      {} bytes", s.postings_bytes);
    println!("term dict:     {} bytes", s.dict_bytes);
}

fn cmd_suggest(dir: &str, field: &str, prefix: &str) {
    let idx = open(dir);
    let snap = idx.snapshot();
    for s in se_core::search::suggest(&snap, field, prefix, 10) {
        println!("{s}");
    }
}

fn pct(mut v: Vec<u128>, p: f64) -> u128 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((p * v.len() as f64) as usize).min(v.len() - 1)]
}

fn cmd_bench(dir: &str, queries_file: &str, iters: usize, threads: usize) {
    let idx = open(dir);
    let text = std::fs::read_to_string(queries_file).unwrap_or_else(|e| {
        eprintln!("cannot read {queries_file}: {e}");
        std::process::exit(1);
    });
    let queries: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect();
    print!("{{\"iters\":{iters},\"threads\":{threads},\"queries\":[");
    for (i, q) in queries.iter().enumerate() {
        if i > 0 {
            print!(",");
        }
        let opts = SearchOptions {
            top_k: 10,
            max_threads: threads,
            ..Default::default()
        };
        // warmup
        for _ in 0..3 {
            let _ = idx.search(q, &opts);
        }
        let mut samples = Vec::with_capacity(iters);
        let mut hits = 0usize;
        for _ in 0..iters {
            let t0 = std::time::Instant::now();
            hits = idx.search(q, &opts).map(|h| h.len()).unwrap_or(0);
            samples.push(t0.elapsed().as_micros());
        }
        let esc_q = q.replace('\\', "\\\\").replace('"', "\\\"");
        print!(
            "{{\"q\":\"{esc_q}\",\"hits\":{hits},\"min_us\":{},\"p50_us\":{},\"p95_us\":{},\"p99_us\":{}}}",
            samples.iter().min().copied().unwrap_or(0),
            pct(samples.clone(), 0.50),
            pct(samples.clone(), 0.95),
            pct(samples, 0.99),
        );
    }
    println!("]}}");
}
