#!/usr/bin/env python3
"""SQLite FTS5 (BM25) comparison on the same corpus + queries (frozen script).

Usage: python3 tools/compare_sqlite.py <corpus.ndjson> <queries.txt>
Prints JSON: index time, index size, per-query latency + hit counts.
"""
import json
import sqlite3
import sys
import time

QUERIES_FTS = None


def to_fts(q):
    # minimal translation of our query language to FTS5
    q = q.strip()
    if q.startswith('"') and q.endswith('"'):
        return q
    if q.endswith("~1") or q.endswith("~2"):
        return q[:-2]
    if q.endswith("*"):
        return q
    if " AND " in q or " OR " in q or " NOT " in q or q.startswith("("):
        out = q.replace(" AND ", " AND ").replace(" OR ", " OR ").replace(" NOT ", " NOT ")
        return out
    if q.startswith("title:"):
        return "{title} : " + q[6:]
    if " " in q:
        return " OR ".join(q.split())
    return q


def main():
    corpus, qfile = sys.argv[1], sys.argv[2]
    docs = [json.loads(l) for l in open(corpus) if l.strip()]
    t0 = time.time()
    con = sqlite3.connect(":memory:")
    con.execute("CREATE VIRTUAL TABLE d USING fts5(title, body)")
    con.executemany("INSERT INTO d(title, body) VALUES (?, ?)",
                    [(d.get("title", ""), d.get("body", "")) for d in docs])
    con.commit()
    index_s = time.time() - t0
    size = con.execute("SELECT SUM(pgsize) FROM dbstat").fetchone()[0] or 0
    queries = [l.strip() for l in open(qfile) if l.strip()]
    out = []
    for q in queries:
        fts = to_fts(q)
        t = time.time()
        try:
            n = len(con.execute(
                "SELECT rowid FROM d WHERE d MATCH ? LIMIT 10", (fts,)).fetchall())
            err = None
        except Exception as e:
            n, err = 0, str(e)[:80]
        dt = (time.time() - t) * 1e6
        out.append({"q": q, "fts": fts, "hits": n, "us": dt, "error": err})
    print(json.dumps({"engine": "sqlite-fts5-bm25", "docs": len(docs),
                      "index_s": index_s, "index_bytes": size, "queries": out}, indent=2))


if __name__ == "__main__":
    main()
