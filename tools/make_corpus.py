#!/usr/bin/env python3
"""Build the benchmark corpus (frozen generator, seed 42).

Tries, in order:
  1. Simple English Wikipedia dump slice (openly licensed CC BY-SA 3.0 /
     GFDL; attribution recorded in README). Downloads
     simplewiki-latest-pages-articles.xml.bz2 (first 200 MB cap).
  2. Falls back to a deterministic synthetic 100k-document corpus.

Usage: python3 tools/make_corpus.py <out.ndjson> [--n 100000]
Writes one JSON object per line: {"title": ..., "body": ...}.
"""
import bz2
import io
import json
import os
import random
import re
import sys
import urllib.request

N_DEFAULT = 100_000
DUMP_URL = "https://dumps.wikimedia.org/simplewiki/latest/simplewiki-latest-pages-articles.xml.bz2"
CAP_BYTES = 200 * 1024 * 1024

VOCAB = [
    "rust", "search", "engine", "fast", "index", "query", "document", "token",
    "merge", "segment", "ranking", "bm25", "phrase", "fuzzy", "prefix",
    "python", "java", "compiler", "database", "network", "server", "client",
    "cache", "queue", "thread", "memory", "disk", "file", "system", "kernel",
    "browser", "protocol", "socket", "request", "response", "parser", "lexer",
    "algorithm", "structure", "array", "tree", "graph", "hash", "sort",
    "history", "science", "music", "film", "city", "river", "mountain",
    "school", "garden", "recipe", "travel", "sport", "health", "space",
]
TITLES = [
    "Rust search engine", "Indexing tutorial", "Query parsing guide",
    "Ranking with BM25", "Merging segments", "Write ahead log",
    "Text analysis basics", "Posting lists", "Full text search",
    "Database internals",
]


def synthetic(path, n):
    rng = random.Random(42)
    with open(path, "w") as f:
        for i in range(n):
            title = f"{rng.choice(TITLES)} {i}"
            body = " ".join(rng.choice(VOCAB) for _ in range(rng.randint(30, 120)))
            f.write(json.dumps({"title": title, "body": body}) + "\n")
    print(f"synthetic corpus: {n} docs -> {path}")
    return "synthetic"


def try_wiki(path, n):
    print(f"downloading {DUMP_URL} (cap {CAP_BYTES // 1024 // 1024} MB) ...", flush=True)
    req = urllib.request.Request(DUMP_URL, headers={"User-Agent": "se-bench/0.1"})
    data = bytearray()
    with urllib.request.urlopen(req, timeout=60) as r:
        while len(data) < CAP_BYTES:
            chunk = r.read(1 << 20)
            if not chunk:
                break
            data += chunk
    print(f"got {len(data)} bytes, parsing ...", flush=True)
    tag_re = re.compile(rb"<title>(.*?)</title>|<text[^>]*>(.*?)</text>", re.S)
    docs = []
    cur_title = None
    # stream-decompress in chunks to avoid huge memory
    dec = bz2.BZ2Decompressor()
    buf = b""
    raw = bytes(data)
    try:
        xml = dec.decompress(raw)
    except Exception as e:
        print(f"decompress failed: {e}")
        return None
    for m in tag_re.finditer(xml):
        t, x = m.group(1), m.group(2)
        if t is not None:
            cur_title = t.decode("utf-8", "ignore")
        elif x is not None and cur_title and ":" not in cur_title:
            body = re.sub(r"\{\{.*?\}\}|\[\[|\]\]|\{\{|\}\}|=+", " ", x.decode("utf-8", "ignore"))
            body = re.sub(r"\s+", " ", body).strip()[:2000]
            if len(body.split()) >= 20:
                docs.append((cur_title, body))
                if len(docs) >= n:
                    break
            cur_title = None
    if len(docs) < 10_000:
        print(f"only {len(docs)} wiki docs parsed, too few")
        return None
    with open(path, "w") as f:
        for t, b in docs[:n]:
            f.write(json.dumps({"title": t, "body": b}) + "\n")
    print(f"simplewiki corpus: {len(docs[:n])} docs -> {path}")
    return "simplewiki"


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else "bench/corpus.ndjson"
    n = int(sys.argv[2]) if len(sys.argv) > 2 else N_DEFAULT
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)
    kind = None
    try:
        kind = try_wiki(out, n)
    except Exception as e:
        print(f"wiki download failed ({e}), using synthetic corpus")
    if kind is None:
        kind = synthetic(out, n)
    size = os.path.getsize(out)
    print(f"corpus_kind={kind} docs={n} bytes={size}")


if __name__ == "__main__":
    main()
