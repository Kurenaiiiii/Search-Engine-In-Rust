#!/usr/bin/env python3
"""BEIR SciFact OR-variant evaluation (labeled follow-up analysis).

The frozen tools/eval_beir.py evaluates raw queries (implicit AND), which
over-constrains verbose BEIR queries and yields ~0 hits. This variant joins
query terms with OR at *evaluation time only* (the engine is unchanged) to
show how much of the gap is query semantics vs ranking. Results are labeled
as a query-rewriting variant and are NOT comparable to published baselines.
"""
import json
import math
import os
import subprocess
import sys

sys.path.insert(0, "tools")
from eval_beir import dcg, ndcg_at, recall_at, search_cli  # noqa


def main():
    work = "bench/beir"
    out = "bench/results/relevance_or.json"
    base = os.path.join(work, "scifact")
    corpus = [json.loads(l) for l in open(os.path.join(base, "corpus.jsonl"))]
    queries = [json.loads(l) for l in open(os.path.join(base, "queries.jsonl"))]
    qrels = {}
    for l in open(os.path.join(base, "qrels", "test.tsv")):
        parts = l.strip().split("\t")
        if len(parts) < 3 or parts[0] == "query-id":
            continue
        qrels.setdefault(parts[0], {})[parts[1]] = int(parts[2])
    pos = {c["_id"]: i for i, c in enumerate(corpus)}
    data_dir = os.path.join(work, "se_data")
    qmap = {q["_id"]: q["text"] for q in queries}
    ndcgs, recs, n, empty = [], [], 0, 0
    for qid, gold in qrels.items():
        if qid not in qmap:
            continue
        gold_int = {pos[d]: s for d, s in gold.items() if d in pos}
        if not gold_int:
            continue
        or_q = " OR ".join(qmap[qid].split())
        pred = search_cli(data_dir, or_q, 100)
        if not pred:
            empty += 1
        ndcgs.append(ndcg_at(pred, gold_int))
        recs.append(recall_at(pred, gold_int))
        n += 1
    res = {"dataset": "BEIR-SciFact OR-variant (eval-time rewrite, engine unchanged)",
           "queries": n, "empty_result_queries": empty,
           "nDCG@10": sum(ndcgs) / len(ndcgs), "Recall@100": sum(recs) / len(recs)}
    json.dump(res, open(out, "w"), indent=2)
    print(json.dumps(res, indent=2))


if __name__ == "__main__":
    main()
