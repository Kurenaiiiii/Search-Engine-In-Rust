#!/usr/bin/env python3
"""Relevance evaluation (frozen script).

Tries to download the BEIR SciFact dataset and evaluate this engine's BM25
(nDCG@10, Recall@100). If the download fails (no network / URL moved), falls
back to a clearly labeled synthetic relevance test.

Usage: python3 tools/eval_beir.py <se_data_dir_with_corpus> [out.json]
The index dir must already contain the BEIR corpus (see tools/index_beir.py
flow inside this script: it indexes corpus.jsonl itself via se-cli).
"""
import json
import math
import os
import subprocess
import sys
import urllib.request
import zipfile

BEIR_URL = "https://public.ukp.informatik.tu-darmstadt.de/thakur/BEIR/datasets/scifact.zip"


def dcg(rel, k):
    return sum((r / math.log2(i + 2)) for i, r in enumerate(rel[:k]))


def ndcg_at(pred, gold, k=10):
    rel = [gold.get(d, 0) for d in pred[:k]]
    ideal = sorted(gold.values(), reverse=True)
    idcg = dcg(ideal, k)
    return dcg(rel, k) / idcg if idcg > 0 else 0.0


def recall_at(pred, gold, k=100):
    if not gold:
        return 0.0
    return len(set(pred[:k]) & set(gold)) / len(gold)


def search_cli(data_dir, query, k=100):
    p = subprocess.run(
        ["./target/release/se-cli", "--dir", data_dir, "search", query,
         "--top-k", str(k), "--json"],
        capture_output=True, text=True)
    try:
        return [h["id"] for h in json.loads(p.stdout)["hits"]]
    except Exception:
        return []


def main():
    work = sys.argv[1] if len(sys.argv) > 1 else "bench/beir"
    out = sys.argv[2] if len(sys.argv) > 2 else "bench/results/relevance.json"
    os.makedirs(work, exist_ok=True)
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    zpath = os.path.join(work, "scifact.zip")
    try:
        print(f"downloading {BEIR_URL} ...", flush=True)
        urllib.request.urlretrieve(BEIR_URL, zpath)
        with zipfile.ZipFile(zpath) as z:
            z.extractall(work)
        base = os.path.join(work, "scifact")
        corpus = [json.loads(l) for l in open(os.path.join(base, "corpus.jsonl"))]
        queries = [json.loads(l) for l in open(os.path.join(base, "queries.jsonl"))]
        qrels = {}
        for l in open(os.path.join(base, "qrels", "test.tsv")):
            parts = l.strip().split("\t")
            if len(parts) < 3 or parts[0] == "query-id":
                continue
            qrels.setdefault(parts[0], {})[parts[1]] = int(parts[2])
        # map beir doc ids to our integer ids
        beir_ids = [c["_id"] for c in corpus]
        pos = {d: i for i, d in enumerate(beir_ids)}
        ndoc = open(os.path.join(work, "corpus.ndjson"), "w")
        for c in corpus:
            ndoc.write(json.dumps({"title": c.get("title", ""), "body": c.get("text", "")}) + "\n")
        ndoc.close()
        data_dir = os.path.join(work, "se_data")
        subprocess.run(["rm", "-rf", data_dir])
        subprocess.run(["./target/release/se-cli", "--dir", data_dir, "index",
                        os.path.join(work, "corpus.ndjson")], check=True)
        qmap = {q["_id"]: q["text"] for q in queries}
        ndcgs, recs, n = [], [], 0
        for qid, gold in qrels.items():
            if qid not in qmap:
                continue
            gold_int = {pos[d]: s for d, s in gold.items() if d in pos}
            if not gold_int:
                continue
            pred = search_cli(data_dir, qmap[qid], 100)
            ndcgs.append(ndcg_at(pred, gold_int))
            recs.append(recall_at(pred, gold_int))
            n += 1
        res = {"dataset": "BEIR-SciFact", "queries": n,
               "nDCG@10": sum(ndcgs) / len(ndcgs), "Recall@100": sum(recs) / len(recs)}
    except Exception as e:
        print(f"BEIR unavailable ({e}); running SYNTHETIC relevance test")
        res = synthetic()
    json.dump(res, open(out, "w"), indent=2)
    print(json.dumps(res, indent=2))


def synthetic():
    # Labeled synthetic: 2000 docs, 50 queries with known relevant sets.
    import random
    rng = random.Random(7)
    vocab = [f"w{i}" for i in range(200)]
    work = "bench/synth_rel"
    os.makedirs(work, exist_ok=True)
    topics = [[f"w{rng.randrange(200)}" for _ in range(3)] for _ in range(50)]
    with open(os.path.join(work, "corpus.ndjson"), "w") as f:
        for i in range(2000):
            words = [rng.choice(vocab) for _ in range(40)]
            if i % 40 == 0:  # plant a relevant doc for topic i//40
                words += topics[(i // 40) % 50]
            f.write(json.dumps({"title": f"doc {i}", "body": " ".join(words)}) + "\n")
    data_dir = os.path.join(work, "se_data")
    subprocess.run(["rm", "-rf", data_dir])
    subprocess.run(["./target/release/se-cli", "--dir", data_dir, "index",
                    os.path.join(work, "corpus.ndjson")],
                   check=True, capture_output=True)
    # relevant = planted docs; query = topic words
    ndcgs, recs = [], []
    for t, topic in enumerate(topics):
        gold = {t * 40: 1}
        pred = search_cli(data_dir, " ".join(topic), 100)
        ndcgs.append(ndcg_at(pred, gold))
        recs.append(recall_at(pred, gold))
    return {"dataset": "SYNTHETIC (seed 7, planted topics — NOT comparable to BEIR)",
            "queries": 50, "nDCG@10": sum(ndcgs) / len(ndcgs),
            "Recall@100": sum(recs) / len(recs)}


if __name__ == "__main__":
    main()
