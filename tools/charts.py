#!/usr/bin/env python3
"""Hand-written SVG charts (no plotting dependencies).

Usage: python3 tools/charts.py
Reads bench/results/{latency_merged_mt,sqlite,tantivy}.json and writes
docs/latency.svg (ours, p50 by query), docs/comparison.svg (p50 across
engines, log-ish via sqrt scale noted on chart), docs/throughput.svg
(indexing docs/s per engine).
"""
import json
import os

RES = "bench/results"
OUT = "docs"


def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def svg_bars(path, title, labels, values, unit, note=""):
    W, pad = 700, 210
    H = 110 + 32 * len(labels)
    mx = max(values) if values else 1
    rows = []
    for i, (lab, val) in enumerate(zip(labels, values)):
        y = 58 + i * 32
        w = (W - pad - 110) * (val / mx)
        rows.append(
            f'<text x="10" y="{y + 13}" font-size="12" font-family="sans-serif">{esc(lab[:30])}</text>'
            f'<rect x="{pad}" y="{y}" width="{w:.1f}" height="19" fill="#4f46e5"/>'
            f'<text x="{pad + w + 6:.0f}" y="{y + 14}" font-size="12" font-family="sans-serif">'
            f"{val:,.0f} {unit}</text>")
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}">'
           f'<text x="10" y="28" font-size="16" font-family="sans-serif">{esc(title)}</text>'
           f'<text x="10" y="46" font-size="11" font-family="sans-serif" fill="#555">{esc(note)}</text>'
           + "\n".join(rows) + "</svg>")
    open(path, "w").write(svg)
    print(f"wrote {path}")


def grouped(path, title, groups, series, note=""):
    # groups: query labels; series: {name: [values], ...} drawn side by side
    W, pad = 760, 190
    names = list(series.keys())
    colors = ["#4f46e5", "#16a34a", "#ea580c", "#0891b2"]
    H = 110 + 30 * len(groups)
    mx = max(max(v) for v in series.values()) if series else 1
    rows = []
    bh = 20 // max(1, len(names))
    for i, g in enumerate(groups):
        y = 58 + i * 30
        rows.append(
            f'<text x="10" y="{y + 13}" font-size="11" font-family="sans-serif">{esc(g[:26])}</text>')
        for j, nm in enumerate(names):
            val = series[nm][i]
            w = (W - pad - 110) * (val / mx)
            yy = y + j * (bh + 1)
            rows.append(
                f'<rect x="{pad}" y="{yy}" width="{max(w, 1):.1f}" height="{bh}" fill="{colors[j % 4]}"/>'
                f'<text x="{pad + w + 5:.0f}" y="{yy + bh}" font-size="10" font-family="sans-serif">{val:,.0f}</text>')
    legend = "".join(
        f'<rect x="{pad + j * 150}" y="30" width="12" height="12" fill="{colors[j % 4]}"/>'
        f'<text x="{pad + j * 150 + 16}" y="40" font-size="11" font-family="sans-serif">{esc(nm)}</text>'
        for j, nm in enumerate(names))
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}">'
           f'<text x="10" y="22" font-size="15" font-family="sans-serif">{esc(title)}</text>'
           + legend +
           f'<text x="10" y="46" font-size="11" font-family="sans-serif" fill="#555">{esc(note)}</text>'
           + "\n".join(rows) + "</svg>")
    open(path, "w").write(svg)
    print(f"wrote {path}")


def main():
    os.makedirs(OUT, exist_ok=True)
    ours = json.load(open(f"{RES}/latency_merged_mt.json"))["queries"]
    lite = json.load(open(f"{RES}/sqlite.json"))["queries"]
    tant = {q["q"]: q for q in json.load(open(f"{RES}/tantivy.json"))["queries"]}
    lite_map = {q["q"]: q for q in lite}
    labels = [q["q"] for q in ours]
    svg_bars(f"{OUT}/latency.svg", "Search Engine in Rust: query latency p50 (µs, lower is better)",
             labels, [q["p50_us"] for q in ours], "µs",
             "100k SimpleWiki docs, merged 1-segment index, 20 iterations")
    # comparison: map queries; sqlite NOT-war errored -> 0; tantivy NOT-war -> 0
    comp_labels, s_se, s_lite, s_tant = [], [], [], []
    for q in ours:
        name = q["q"]
        if name not in lite_map or name not in tant:
            continue
        comp_labels.append(name)
        s_se.append(q["p50_us"])
        s_lite.append(int(lite_map[name]["us"] if not lite_map[name]["error"] else 0))
        s_tant.append(int(tant[name]["p50_us"] if tant[name]["hits"] >= 0 else 0))
    grouped(f"{OUT}/comparison.svg", "Query latency p50 (µs): engine comparison",
            comp_labels,
            {"search-engine-in-rust": s_se, "sqlite-fts5": s_lite, "tantivy": s_tant},
            "same corpus, same queries, top-10; 0 = unsupported query syntax")
    se_docs = 100_000 / 185.17
    svg_bars(f"{OUT}/throughput.svg", "Indexing throughput (docs/s, higher is better)",
             ["tantivy", "sqlite-fts5", "search-engine-in-rust"],
             [100_000 / 3.3, 100_000 / 11.1, se_docs], "docs/s",
             "100k SimpleWiki docs on this machine (see docs/BENCHMARKS.md)")


if __name__ == "__main__":
    main()
