#!/usr/bin/env python3
"""Hand-written SVG charts (no plotting dependencies).

Usage: python3 tools/charts.py <latency.json> <outdir>
Writes latency.svg (p50 by query type) and throughput.svg (indexing bars).
"""
import json
import sys


def svg_bar(path, title, labels, values, unit):
    W, H, pad = 640, 120 + 34 * len(labels), 200
    mx = max(values) if values else 1
    rows = []
    for i, (lab, val) in enumerate(zip(labels, values)):
        y = 60 + i * 34
        w = (W - pad - 90) * (val / mx)
        rows.append(
            f'<text x="10" y="{y + 13}" font-size="12" font-family="sans-serif">{lab[:28]}</text>'
            f'<rect x="{pad}" y="{y}" width="{w:.1f}" height="20" fill="#4f46e5"/>'
            f'<text x="{pad + w + 6:.0f}" y="{y + 15}" font-size="12" font-family="sans-serif">'
            f"{val:,.0f} {unit}</text>")
    body = "\n".join(rows)
    svg = (f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}">'
           f'<text x="10" y="30" font-size="16" font-family="sans-serif">{title}</text>{body}</svg>')
    open(path, "w").write(svg)
    print(f"wrote {path}")


def main():
    lat_path, outdir = sys.argv[1], sys.argv[2]
    lat = json.load(open(lat_path))
    labels = [q["q"] for q in lat["queries"]]
    p50 = [q["p50_us"] for q in lat["queries"]]
    svg_bar(f"{outdir}/latency.svg", "Query latency p50 (microseconds, lower is better)",
            labels, p50, "µs")


if __name__ == "__main__":
    main()
