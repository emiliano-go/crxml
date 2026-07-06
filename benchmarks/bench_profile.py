#!/usr/bin/env python3
"""
Phase 0 profiling — emit bench_results/<sha>.json with measured breakdown.

Usage (profile build required):
    pip install -e . --config-settings=--features=profile
    python bench_profile.py

Output:
    bench_results/<git-sha>.json   — per-file profile data
    bench_results/latest.json      — symlink to most recent run
"""

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
BENCH_DATA = ROOT / "benchmarks" / "bench_data"
BENCH_RESULTS = ROOT / "benchmarks" / "bench_results"

FILES = [
    ("10 MB", "test_10mb.xml"),
    ("50 MB", "test_50mb.xml"),
    ("100 MB", "test_100mb.xml"),
]


def get_git_sha() -> str:
    return subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()


def run_reader(path: str, row_tag: str = "Details") -> dict:
    from crxml import _crxml_core as _core

    reader = _core.CrxmlReader(path, row_tag)
    t0 = time.perf_counter()
    count = 0
    for _ in reader:
        count += 1
    wall = time.perf_counter() - t0

    data = reader.get_profile_data()
    data["row_count"] = count
    data["wall_ns"] = int(wall * 1e9)
    data["wall_ms"] = wall * 1000
    return data


def run_parallel(path: str, row_tag: str = "Details") -> dict:
    import gc
    from crxml import CrystalXMLSource, _crxml_core as _core

    # Warmup: parse once, discard
    gc.collect()
    _ = CrystalXMLSource(path, row_tag=row_tag, engine="parallel", use_mmap=True, auto_dict=False).to_dataframe()
    del _
    gc.collect()

    t0 = time.perf_counter()
    src = CrystalXMLSource(path, row_tag=row_tag, engine="parallel", use_mmap=True, auto_dict=False)
    df = src.to_dataframe()
    wall = time.perf_counter() - t0
    rows = len(df)
    del src, df

    profile = _core.get_par_profile()
    total_ns = profile["split_scan_ns"] + profile["parse_ns"] + profile["assembly_export_ns"]
    wall_ns = int(wall * 1e9)
    coverage = round(total_ns * 100 / max(wall_ns, 1), 1)
    data = {
        "row_count": rows,
        "wall_ns": wall_ns,
        "wall_ms": wall * 1000,
        "engine": "parallel",
        "split_scan_ns": profile["split_scan_ns"],
        "parse_ns": profile["parse_ns"],
        "assembly_export_ns": profile["assembly_export_ns"],
        "profile_coverage_pct": coverage,
    }

    # auto_dict comparison (also warm)
    gc.collect()
    _ = CrystalXMLSource(path, row_tag=row_tag, engine="parallel", use_mmap=True, auto_dict=True).to_dataframe()
    del _
    gc.collect()
    t0 = time.perf_counter()
    src = CrystalXMLSource(path, row_tag=row_tag, engine="parallel", use_mmap=True, auto_dict=True)
    df2 = src.to_dataframe()
    wall_ad = time.perf_counter() - t0
    profile_ad = _core.get_par_profile()
    total_ad = profile_ad["split_scan_ns"] + profile_ad["parse_ns"] + profile_ad["assembly_export_ns"]
    data["auto_dict"] = {
        "wall_ms": wall_ad * 1000,
        "wall_ns": int(wall_ad * 1e9),
        "split_scan_ns": profile_ad["split_scan_ns"],
        "parse_ns": profile_ad["parse_ns"],
        "assembly_export_ns": profile_ad["assembly_export_ns"],
        "profile_coverage_pct": round(total_ad * 100 / max(int(wall_ad * 1e9), 1), 1),
    }

    return data


def run_pyspy(path: str, duration: float = 5.0) -> dict:
    pyspy_bin = shutil.which("py-spy") or str(Path(sys.executable).parent / "py-spy")
    flamegraph_path = BENCH_RESULTS / "flamegraph.svg"

    script = (
        "from crxml import _crxml_core as _core\n"
        f"reader = _core.CrxmlReader('{path}', 'Details')\n"
        "for _ in reader: pass\n"
    )
    cmd = [
        pyspy_bin, "record",
        "-o", str(flamegraph_path),
        "--native",
        "--duration", str(int(duration)),
        "--", sys.executable, "-c", script,
    ]
    try:
        cp = subprocess.run(cmd, capture_output=True, text=True,
                            timeout=duration + 30)
        if cp.returncode != 0:
            return {"error": f"py-spy exited {cp.returncode}: {cp.stderr.strip()}"}
    except Exception as exc:
        return {"error": str(exc)}

    if not flamegraph_path.exists():
        return {"error": "flamegraph not generated"}

    size_kb = round(flamegraph_path.stat().st_size / 1024)
    return {
        "flamegraph_svg": str(flamegraph_path),
        "size_kb": size_kb,
        "duration_s": duration,
    }


def main():
    BENCH_RESULTS.mkdir(parents=True, exist_ok=True)
    sha = get_git_sha()

    results = {
        "meta": {
            "git_sha": sha,
            "timestamp": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "profile_feature": True,
            "python": sys.version,
        },
        "files": {},
    }

    enabled = []
    for label, filename in FILES:
        path = BENCH_DATA / filename
        if not path.exists():
            print(f"  [SKIP] {label} - {filename} not found, run benchmarks/benchmarks.py first")
            continue
        enabled.append((label, str(path)))

    if not enabled:
        print("No benchmark files found. Generate them with: python benchmarks/benchmarks.py --gen-only")
        sys.exit(1)

    print(f"Benchmarking commit {sha} (profile build)\n")

    for label, path in enabled:
        print(f"  {label} ... ", end="", flush=True)
        data = run_reader(path)
        print(f'[stream] {data["row_count"]} rows, {data["wall_ms"]:.1f} ms wall')
        total_ns = data["event_loop_ns"] + data["unescape_ns"] + data["dict_build_ns"]
        if total_ns > 0:
            print(f'    event_loop: {data["event_loop_ns"]/1e6:.2f} ms'
                  f' ({data["event_loop_ns"]*100/total_ns:.1f}%)')
            print(f'    unescape:   {data["unescape_ns"]/1e6:.2f} ms'
                  f' ({data["unescape_ns"]*100/total_ns:.1f}%)')
            print(f'    dict_build: {data["dict_build_ns"]/1e6:.2f} ms'
                  f' ({data["dict_build_ns"]*100/total_ns:.1f}%)')
            print(f'    sum→wall: {total_ns/1e6:.1f}/{data["wall_ms"]:.1f} ms'
                  f' ({total_ns*100/data["wall_ns"]:.0f}% coverage)')

        par = run_parallel(path)
        print(f'  {label} [parallel] {par["row_count"]} rows, {par["wall_ms"]:.1f} ms wall')
        print(f'    split_scan: {par["split_scan_ns"]/1e6:.2f} ms'
              f' ({par["split_scan_ns"]*100/total_ns:.1f}%)'
              if total_ns > 0 else '')
        print(f'    parse:      {par["parse_ns"]/1e6:.2f} ms')
        print(f'    GIL:        {par["assembly_export_ns"]/1e6:.2f} ms'
              f' ({par["assembly_export_ns"]*100/total_ns:.1f}%)'
              if total_ns > 0 else '')
        print(f'    coverage:   {par["profile_coverage_pct"]}%')
        if "auto_dict" in par:
            ad = par["auto_dict"]
            print(f'    auto_dict:  {ad["wall_ms"]:.1f} ms wall'
                  f' (split={ad["split_scan_ns"]/1e6:.1f}'
                  f' parse={ad["parse_ns"]/1e6:.1f}'
                  f' gil={ad["assembly_export_ns"]/1e6:.1f})')

        results["files"][label] = {"stream": data, "parallel": par}

    results["pyspy"] = {"note": "skipped — install py-spy and uncomment in source"}

    output_path = BENCH_RESULTS / f"{sha}.json"
    with open(output_path, "w") as f:
        json.dump(results, f, indent=2, default=str)
    print(f"\n  → {output_path}")

    latest = BENCH_RESULTS / "latest.json"
    if latest.exists() or latest.is_symlink():
        latest.unlink()
    latest.symlink_to(f"{sha}.json")
    print(f"  → {latest} (symlink)")


if __name__ == "__main__":
    main()
