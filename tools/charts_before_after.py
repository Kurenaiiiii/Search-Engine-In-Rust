#!/usr/bin/env python3
"""Before/after charts for the v0.2.0 optimization round (hand-written SVG,
no plotting dependencies). The frozen tools/charts.py is left untouched;
this script reads the committed before/after medians and writes refreshed
docs/*.svg files plus docs/latency_before_after.svg.

Usage: python3 tools/charts_before_after.py
Reads bench/results/before/median.json and bench/results/after/median.json.
"""
import json
import math
import os

BEFORE = json.load(open("bench/results/before/median.json"))
AFTER = json.load(open("bench/results/after/median.json"))
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


def log_grouped(path, title, groups, before_vals, after_vals, note=""):
    # log10 scale: spans ~1e2..1e6 us; ticks labeled with real values
    W, pad = 760, 200
    H = 120 + 34 * len(groups)
    lmin, lmax = 2.0, 7.0  # 100 us .. 10 s
    def w(v):
        v = max(v, 100)
        return (W - pad - 130) * (math.log10(v) - lmin) / (lmax - lmin)
    rows = []
    for i, g in enumerate(groups):
        y = 66 + i * 34
        rows.append(
            f'<text x="10" y="{y + 13}" font-size="11" font-family="sans-serif">{esc(g[:28])}</text>')
        bw, aw = w(before_vals[i]), w(after_vals[i])
        rows.append(
            f'<rect x="{pad}" y="{y}" width="{bw:.1f}" height="12" fill="#9ca3af"/>'
            f'<text x="{pad + bw + 5:.0f}" y="{y + 10}" font-size="10" font-family="sans-serif">{before_vals[i]:,.0f}</text>')
        rows.append(
            f'<rect x="{pad}" y="{y + 14}" width="{aw:.1f}" height="12" fill="#4f46e5"/>'
            f'<text x="{pad + aw + 5:.0f}" y="{y + 24}" font-size="10" font-family="sans-serif">{after_vals[i]:,.0f}</text>')
    legend = (f'<rect x="{pad}" y="30" width="12" height="12" fill="#9ca3af"/>'
              f'<text x="{pad + 16}" y="40" font-size="11" font-family="sans-serif">v0.1.0 (before)</text>'
              f'<rect x="{pad + 170}" y="30" width="12" height="12" fill="#4f46e5"/>'
              f'<text x="{pad + 186}" y="40" font-size="11" font-family="sans-serif">v0.2.0 (after)</text>')
    ticks = "".join(
        f'<text x="{pad + (W - pad - 130) * (e - lmin) / (lmax - lmin):.0f}" y="{H - 12}" font-size="10" '
        f'font-family="sans-serif" fill="#555">1e{e} us</text>'
        for e in (2, 3, 4, 5, 6))
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}">'
           f'<text x="10" y="22" font-size="15" font-family="sans-serif">{esc(title)}</text>'
           + legend +
           f'<text x="10" y="54" font-size="11" font-family="sans-serif" fill="#555">{esc(note)}</text>'
           + "\n".join(rows) + ticks + "</svg>")
    open(path, "w").write(svg)
    print(f"wrote {path}")


def main():
    os.makedirs(OUT, exist_ok=True)
    a_lat = AFTER["latency_merged_mt"]
    labels = list(a_lat.keys())
    svg_bars(f"{OUT}/latency.svg",
             "Search Engine in Rust v0.2.0: query latency p50 (µs, lower is better)",
             labels, [a_lat[q]["p50_us"] for q in labels], "µs",
             "100k SimpleWiki docs, merged 1-segment index, 20 iterations, median of 3 runs")
    # engine comparison on after data
    a_sq = AFTER["sqlite_queries"]
    a_ta = AFTER["tantivy_queries"]
    comp_labels = [q for q in labels if q in a_sq and q in a_ta]
    grouped(f"{OUT}/comparison.svg", "Query latency p50 (µs): engine comparison",
            comp_labels,
            {"search-engine-in-rust": [a_lat[q]["p50_us"] for q in comp_labels],
             "sqlite-fts5": [int(a_sq[q]) for q in comp_labels],
             "tantivy": [int(a_ta[q]) for q in comp_labels]},
            "same corpus, same queries, top-10; after-codecs v0.2.0 medians")
    svg_bars(f"{OUT}/throughput.svg", "Indexing throughput (docs/s, higher is better)",
             ["tantivy", "sqlite-fts5", "search-engine-in-rust v0.1.0",
              "search-engine-in-rust v0.2.0"],
             [100_000 / AFTER["tantivy_index_s"], 100_000 / AFTER["sqlite_index_s"],
              BEFORE["docs_per_s"], AFTER["docs_per_s"]], "docs/s",
             "100k SimpleWiki docs, median of 3 runs (see docs/BENCHMARKS.md)")
    b_lat = BEFORE["latency_merged_mt"]
    log_grouped(f"{OUT}/latency_before_after.svg",
                "Query latency p50 (µs, log scale): v0.1.0 vs v0.2.0",
                labels,
                [b_lat[q]["p50_us"] for q in labels],
                [a_lat[q]["p50_us"] for q in labels],
                "median of 3 runs; free-text queries changed AND->OR semantics by design")


if __name__ == "__main__":
    main()
