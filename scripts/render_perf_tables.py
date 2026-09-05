#!/usr/bin/env python3
"""Render performance markdown tables from benchmark JSON.

Reads .benchmarks/*.json (as produced by bench_extended.py --output)
and writes tables into docs/performance.md between markers.

Usage:
    python benchmarks/bench_extended.py --rounds 7 --output .benchmarks/crxml-1gb.json
    python scripts/render_perf_tables.py
    # in CI:
    python scripts/render_perf_tables.py --check  # fails if docs/performance.md is stale
"""

import argparse
import json
import re
import statistics
from collections import defaultdict
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent
DOCS_PERF = PROJECT_ROOT / "docs" / "performance.md"
BENCH_DIR = PROJECT_ROOT / ".benchmarks"

MARKER_START = "<!-- BEGIN:{name} -->"
MARKER_END = "<!-- END:{name} -->"


def load_jsons(pattern="*.json"):
    files = sorted(BENCH_DIR.glob(pattern))
    data = []
    for f in files:
        try:
            data.append(json.loads(f.read_text(encoding="utf-8")))
        except Exception as e:
            print(f"warn: {f} failed {e}")
    return data


def merge_results(all_data):
    """Merge results across JSON files.

    Returns dict: label -> list of records (one per file size).
    Each record has: size, mb_per_s, rows, cov, median, best, worst, times, rss_mb.
    """
    merged = defaultdict(list)
    for doc in all_data:
        for r in doc.get("results", []):
            label = r["label"]
            merged[label].append({
                "size": r.get("size", 0),
                "mb_per_s": r.get("mb_per_s", 0),
                "rows": r.get("rows", 0),
                "cov": r.get("cov", 0),
                "median": r.get("median", 0),
                "best": r.get("best", 0),
                "worst": r.get("worst", 0),
                "times": r.get("times", []),
                "rss_mb": r.get("rss_mb", 0),
            })
    return dict(merged)


def _fmt_mb(val):
    """Format MB/s as integer."""
    return f"{val:.0f}" if val else "N/A"


def _fmt_rows(val):
    """Format row count: 450000 -> 450k, 4500000 -> 4.50M."""
    if not val:
        return ":"
    if val >= 1_000_000:
        return f"{val / 1_000_000:.2f}M"
    if val >= 1_000:
        return f"{val / 1_000:.0f}k"
    return str(val)


def _fmt_cell(mb, rows, cov, bold=False):
    """Format a table cell: MB/s / rows/s, with optional bold and CoV marker."""
    cell = f"{_fmt_mb(mb)} / {_fmt_rows(rows)}"
    if bold:
        cell = f"**{cell}**"
    if cov > 0.10:
        cell += "†"
    return cell


def _file_label(size_bytes):
    """Convert file size in bytes to label like '100 MB'."""
    mb = size_bytes / (1024 * 1024)
    if mb >= 1000:
        return f"{mb / 1024:.0f} GB"
    return f"{mb:.0f} MB"


# ---------------------------------------------------------------------------
# Table renderers
# ---------------------------------------------------------------------------

def render_native_table(merged):
    """Render native exports table.

    Labels: native single, native par4, native par8, native par16,
            native par32, native par_auto, native bounded64, native bounded256
    """
    # Collect all file sizes across all native results
    all_sizes = set()
    native_results = {}
    for label, records in merged.items():
        if not label.startswith("native "):
            continue
        engine = label[len("native "):]
        native_results[engine] = {}
        for r in records:
            size = r["size"]
            all_sizes.add(size)
            native_results[engine][size] = r

    if not native_results:
        return None

    sizes = sorted(all_sizes)
    # Pick engines to show: single, par_auto (or par16), bounded64
    show_engines = []
    for eng in ["single", "par_auto", "par16", "par32", "bounded64"]:
        if eng in native_results:
            show_engines.append(eng)

    # Build header
    header_cols = ["File"] + [e for e in show_engines]
    header = "| " + " | ".join(header_cols) + " |"
    sep = "|---" * len(header_cols) + "|"

    # Build rows
    rows = []
    for size in sizes:
        cols = [f"**{_file_label(size)}**"]
        # Find peak MB/s in this row for bolding
        cell_vals = []
        for eng in show_engines:
            r = native_results.get(eng, {}).get(size)
            if r:
                cell_vals.append(r["mb_per_s"])
            else:
                cell_vals.append(0)
        peak = max(cell_vals) if cell_vals else 0

        for i, eng in enumerate(show_engines):
            r = native_results.get(eng, {}).get(size)
            if r:
                is_peak = r["mb_per_s"] == peak and peak > 0
                cols.append(_fmt_cell(r["mb_per_s"], r["rows"], r["cov"], bold=is_peak))
            else:
                cols.append("-")
        rows.append("| " + " | ".join(cols) + " |")

    return "\n".join([header, sep] + rows)


def render_source_table(merged):
    """Render source engines x sinks table.

    Labels: src stream -> iter, src columnar -> to_arrow, etc.
    """
    # Collect all file sizes and engine/sink combos
    all_sizes = set()
    combos = {}  # (engine, sink) -> {size: record}
    for label, records in merged.items():
        if not label.startswith("src "):
            continue
        # Parse "src engine -> sink"
        m = re.match(r"src\s+(\S+)\s+->\s+(\S+)", label)
        if not m:
            continue
        engine, sink = m.group(1), m.group(2)
        key = (engine, sink)
        combos[key] = {}
        for r in records:
            size = r["size"]
            all_sizes.add(size)
            combos[key][size] = r

    if not combos:
        return None

    sizes = sorted(all_sizes)
    # Show key combos
    show = []
    for eng in ["stream", "columnar", "parallel", "auto"]:
        for sink in ["iter", "to_arrow", "to_pandas"]:
            if (eng, sink) in combos:
                show.append((eng, sink))

    if not show:
        return None

    header_cols = ["Engine -> Sink"] + [_file_label(s) for s in sizes]
    header = "| " + " | ".join(header_cols) + " |"
    sep = "|---" * len(header_cols) + "|"

    rows = []
    for eng, sink in show:
        cols = [f"**{eng} -> {sink}**"]
        vals = []
        for size in sizes:
            r = combos.get((eng, sink), {}).get(size)
            vals.append(r["mb_per_s"] if r else 0)
        peak = max(vals) if vals else 0

        for i, size in enumerate(sizes):
            r = combos.get((eng, sink), {}).get(size)
            if r:
                is_peak = r["mb_per_s"] == peak and peak > 0
                cols.append(_fmt_cell(r["mb_per_s"], r["rows"], r["cov"], bold=is_peak))
            else:
                cols.append("-")
        rows.append("| " + " | ".join(cols) + " |")

    return "\n".join([header, sep] + rows)


def render_pushdown_table(merged):
    """Render pushdown comparison table (parallel engine only).

    Labels: push baseline [parallel], push drop_half [parallel], etc.
    """
    all_sizes = set()
    push_results = {}
    for label, records in merged.items():
        if not label.startswith("push ") or "[parallel]" not in label:
            continue
        # Extract pushdown name
        m = re.match(r"push\s+(\S+)\s+\[parallel\]", label)
        if not m:
            continue
        pd_name = m.group(1)
        push_results[pd_name] = {}
        for r in records:
            size = r["size"]
            all_sizes.add(size)
            push_results[pd_name][size] = r

    if not push_results:
        return None

    sizes = sorted(all_sizes)
    show = ["baseline", "drop_half", "rename", "schema", "filter_eq", "filter_compare", "auto_dict"]

    header_cols = ["Pushdown [parallel]"] + [_file_label(s) for s in sizes]
    header = "| " + " | ".join(header_cols) + " |"
    sep = "|---" * len(header_cols) + "|"

    rows = []
    for pd in show:
        if pd not in push_results:
            continue
        cols = [f"`{pd}`"]
        vals = []
        for size in sizes:
            r = push_results.get(pd, {}).get(size)
            vals.append(r["mb_per_s"] if r else 0)
        peak = max(vals) if vals else 0

        for size in sizes:
            r = push_results.get(pd, {}).get(size)
            if r:
                is_peak = r["mb_per_s"] == peak and peak > 0
                cols.append(_fmt_cell(r["mb_per_s"], r["rows"], r["cov"], bold=is_peak))
            else:
                cols.append("-")
        rows.append("| " + " | ".join(cols) + " |")

    return "\n".join([header, sep] + rows)


# ---------------------------------------------------------------------------
# Doc update
# ---------------------------------------------------------------------------

def update_doc(markers, dry_run=False):
    text = DOCS_PERF.read_text(encoding="utf-8")
    original = text
    for name, table in markers.items():
        start = MARKER_START.format(name=name)
        end = MARKER_END.format(name=name)
        pattern = re.compile(re.escape(start) + r".*?" + re.escape(end), re.DOTALL)
        replacement = f"{start}\n{table}\n{end}"
        if start in text and end in text:
            text = pattern.sub(replacement, text)
        else:
            # If markers not present, append at end
            text += f"\n{replacement}\n"
    if dry_run:
        if text != original:
            print("docs/performance.md is stale (would change)")
            import difflib
            for line in difflib.unified_diff(
                original.splitlines(), text.splitlines(), lineterm=""
            ):
                print(line)
            return 1
        else:
            print("docs/performance.md is up to date")
            return 0
    else:
        DOCS_PERF.write_text(text, encoding="utf-8")
        print(f"Updated {DOCS_PERF}")
        return 0


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="Render perf tables from JSON")
    ap.add_argument("--check", action="store_true", help="Check if docs are up to date")
    ap.add_argument("--input", type=str, default=".benchmarks/*.json", help="Glob for JSON files")
    args = ap.parse_args()

    all_data = load_jsons()
    if not all_data:
        if args.check:
            print("No JSON files found, nothing to check")
            return 0
        print("No JSON files found in .benchmarks/")
        return 1

    merged = merge_results(all_data)
    print(f"Loaded {len(all_data)} JSON file(s), {sum(len(v) for v in merged.values())} records")

    markers = {}

    native = render_native_table(merged)
    if native:
        markers["native"] = native
        print(f"  native: {len([k for k in merged if k.startswith('native ')])} configs")

    source = render_source_table(merged)
    if source:
        markers["source"] = source
        print(f"  source: {len([k for k in merged if k.startswith('src ')])} configs")

    pushdown = render_pushdown_table(merged)
    if pushdown:
        markers["pushdown"] = pushdown
        print(f"  pushdown: {len([k for k in merged if k.startswith('push ')])} configs")

    if not markers:
        print("No renderable results found")
        return 1

    return update_doc(markers, dry_run=args.check)


if __name__ == "__main__":
    raise SystemExit(main())
