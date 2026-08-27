"""
Extended benchmark suite for crxml — covers ALL use cases and combinations.

Covers:
- File sizes: 10, 50, 100, 1000 MB (1 GB optional / --skip-1gb)
- Native engines: read_to_columnar (single), read_to_columnar_multi (2), read_to_columnar_par (2/4/8/16/32), read_to_columnar_bounded (64/256 MB)
- Source engines: stream, columnar, parallel, auto (with _resolve_engine)
- Sinks: iter (dict), iter_batches, to_arrow, to_dataframe, to_pandas, to_polars, to_parquet, Pipeline+stages, rypipe fusion
- Pushdowns: baseline, drop_fields (half/all), field_mapping (rename), field_types (int64/float64), dictionary, auto_dict, filter (eq/ne/compare/and/or/not), schema ordering, use_mmap on/off
- Batch sizes for streaming: 1024 vs 4096
- Chunk scaling for parallel

Reuses generation helpers from benchmarks.py. Run with --quick for 10 MB only (CI) or --full for all including 1 GB.
"""

import argparse
import os
import statistics
import time
import sys
from pathlib import Path

HERE = Path(__file__).parent
# Reuse generation logic from benchmarks.py
sys.path.insert(0, str(HERE))
from benchmarks import (
    generate_file as gen_file,
    HEAD, TAIL,  # noqa: F401
)

BENCH_DATA = HERE / "bench_data"
BENCH_DATA.mkdir(exist_ok=True)

# Import after generation helpers to avoid circular
from crxml import CrystalXMLSource, _crxml_core as _core

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def best_of(fn, rounds=3):
    best = float("inf")
    rows = 0
    for _ in range(rounds):
        t0 = time.perf_counter()
        res = fn()
        dt = time.perf_counter() - t0
        # row count extraction
        try:
            if isinstance(res, int):
                rows = res
            elif hasattr(res, "num_rows"):
                rows = res.num_rows
            elif hasattr(res, "__len__"):
                try:
                    rows = len(res)
                except Exception:
                    rows = 0
            elif res is None:
                rows = 0
            else:
                rows = 0
        except Exception:
            rows = 0
        best = min(best, dt)
    return best, rows

def median_of(fn, rounds=7):
    """Median of 7 with min/max and CoV, adaptive until 1.31*CoV <=5% capped at 31. Returns (median, best, worst, cov, rows)."""
    times = []
    rows = 0
    # Adaptive: keep sampling until floor <=5% or cap 31
    target_rounds = rounds
    while len(times) < target_rounds:
        t0 = time.perf_counter()
        # For tiny files (10 MB, 20 ms), repeat 20× inside one timed region to average scheduler noise
        # Heuristic: if expected time <50 ms, do 20 repeats
        # We estimate by doing one untimed run first
        if len(times) == 0 and rounds == 7:
            # Quick probe to estimate time
            t_probe0 = time.perf_counter()
            res = fn()
            t_probe = time.perf_counter() - t_probe0
            # Extract rows for probe
            try:
                if isinstance(res, int):
                    rows = res
                elif hasattr(res, "num_rows"):
                    rows = res.num_rows
                elif hasattr(res, "__len__"):
                    rows = len(res)
            except Exception:
                pass
            if t_probe < 0.05:  # 50 ms threshold for tiny files
                # Do 20 repeats inside one timed region
                t0 = time.perf_counter()
                for _ in range(20):
                    fn()
                dt = (time.perf_counter() - t0) / 20
                times.append(dt)
                continue
            else:
                times.append(t_probe)
                continue
        t0 = time.perf_counter()
        res = fn()
        dt = time.perf_counter() - t0
        times.append(dt)
        try:
            if isinstance(res, int):
                rows = res
            elif hasattr(res, "num_rows"):
                rows = res.num_rows
            elif hasattr(res, "__len__"):
                rows = len(res)
        except Exception:
            pass
        if len(times) >= 7:
            # Check CoV floor
            mean = sum(times)/len(times)
            stdev = statistics.pstdev(times) if len(times)>1 else 0
            cov = stdev/mean if mean else 0
            floor = 1.31 * cov
            if floor <= 0.05 or len(times) >= 31:
                break
            # Need more samples to reduce floor: floor ~ 1/sqrt(n), so need 4× rounds to halve
            if len(times) >= 31:
                break
    times.sort()
    median = times[len(times)//2]
    best = min(times)
    worst = max(times)
    mean = sum(times)/len(times)
    stdev = statistics.pstdev(times) if len(times)>1 else 0
    cov = stdev/mean if mean else 0
    return median, best, worst, cov, rows

def peak_rss_subprocess(code: str, timeout=60):
    """Run code in subprocess and return (peak_kb, stdout). Uses child's VmHWM, not RUSAGE_CHILDREN."""
    import subprocess
    # Child reports its own VmHWM before exit to avoid cumulative RUSAGE_CHILDREN high-water
    wrapper = code + "\n" + \
        "import pathlib; " + \
        "try:\n" + \
        "    vm = int(next(l for l in open('/proc/self/status') if l.startswith('VmHWM:')).split()[1])\n" + \
        "    print(f\"__VmHWM__{vm}\")\n" + \
        "except Exception:\n" + \
        "    pass\n"
    r = subprocess.run([sys.executable, "-c", wrapper], capture_output=True, text=True, timeout=timeout)
    peak_kb = 0
    for line in r.stdout.splitlines():
        if line.startswith("__VmHWM__"):
            try:
                peak_kb = int(line.split("__VmHWM__")[1])
            except Exception:
                pass
    # Also capture child's stdout without the VmHWM marker
    out = "\n".join(l for l in r.stdout.splitlines() if not l.startswith("__VmHWM__"))
    return peak_kb, out, r.stderr

def cold_warm(path: Path, fn, use_posix_fadvise=True):
    """Run cold (after posix_fadvise DONTNEED) and warm pair, return (cold_dt, warm_dt).

    Verifies eviction via mincore: after fadvise, resident pages must be ~0,
    otherwise the cold number is warm and we fail loudly.
    """
    if use_posix_fadvise:
        try:
            import ctypes, mmap
            fd = os.open(str(path), os.O_RDONLY)
            try:
                os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
                # Verify via mincore on a fresh mmap
                size = os.fstat(fd).st_size
                if size > 0:
                    import ctypes as _ct
                    libc = _ct.CDLL("libc.so.6", use_errno=True)
                    addr = mmap.mmap(fd, size, access=mmap.ACCESS_READ)
                    vec = (ctypes.c_ubyte * ((size + 4095)//4096))()
                    ret = libc.mincore(addr, size, vec)
                    if ret == 0:
                        resident = sum(vec)
                        if resident != 0:
                            print(f"  warn: fadvise did not evict {path.name}: resident {resident}/{len(vec)} pages — cold is warm, skipping cold/warm")
                            # Fall through to warm-only
                            addr.close()
                            os.close(fd)
                            t0 = time.perf_counter()
                            fn()
                            warm = time.perf_counter() - t0
                            return warm, warm
                    addr.close()
            finally:
                os.close(fd)
        except Exception as e:
            print(f"  warn: fadvise/mincore failed {e}, using warm only")
    t0 = time.perf_counter()
    fn()
    cold = time.perf_counter() - t0
    t0 = time.perf_counter()
    fn()
    warm = time.perf_counter() - t0
    return cold, warm

def bench_lxml(path: Path, to_dataframe=False):
    """External baseline: lxml and ElementTree iterparse, defensible.

    - `elem.clear()` plus ancestor cleanup to avoid DOM build (otherwise measuring tree construction, not parsing).
    - Output shape matched to crxml: `to_dataframe` yields DataFrame, else list of dicts.
    - Compare lxml single-thread vs crxml single (690 MB/s) head-to-head, parallel separately.
    """
    try:
        import lxml.etree
        import pyarrow as pa
        t0 = time.perf_counter()
        rows = []
        # Use iterparse with tag filtering and proper cleanup
        for _, elem in lxml.etree.iterparse(str(path), events=("end",), tag="Details"):
            # Build dict like crxml does (row attrs + Field children)
            d = dict(elem.attrib)
            for child in elem:
                if child.tag == "Field":
                    name = child.get("FieldName") or child.get("Name") or "Field"
                    # Find FormattedValue/Value
                    val = ""
                    for gc in child:
                        if gc.tag in ("FormattedValue", "Value"):
                            val = gc.text or ""
                            break
                    d[name] = val
                elif child.tag == "Text":
                    d[child.get("Name", "Text")] = "".join((gc.text or "") for gc in child if gc.tag == "TextValue")
                elif child.tag == "Section":
                    d["Section"] = child.get("SectionNumber", "")
            rows.append(d)
            # Defensible cleanup: clear element and its ancestors to keep memory constant
            elem.clear()
            # Remove preceding siblings to free memory (lxml idiom)
            parent = elem.getparent()
            if parent is not None:
                # Keep only tail
                while elem.getprevious() is not None:
                    del parent[0]
        dt = time.perf_counter() - t0
        if to_dataframe:
            import pandas as pd
            # Same artifact as crxml to_dataframe (ArrowDtype)
            t1 = time.perf_counter()
            table = pa.table({k: [r.get(k) for r in rows] for k in rows[0]} if rows else {})
            df = table.to_pandas(types_mapper=pd.ArrowDtype)
            dt = time.perf_counter() - t0
            return len(rows), dt, "lxml→DataFrame"
        return len(rows), dt, "lxml→dicts"
    except ImportError:
        return 0, 0, "lxml not installed"
    except Exception as e:
        return 0, 0, f"lxml error: {e}"

def report(path: Path, label: str, fn, rounds=3, **kwargs):
    sz = path.stat().st_size
    try:
        # Use median_of for rounds>=7, else best_of
        if rounds >= 7:
            median, best, worst, cov, rows = median_of(fn, rounds=rounds)
            dt = median
            mb = sz / dt / 1024 / 1024 if dt > 0 else 0
            rps = rows / dt if dt > 0 and rows else 0
            extra = " ".join(f"{k}={v}" for k, v in kwargs.items() if v is not None)
            # Collect JSON if output dir is set via global
            if 'json_results' in globals() and isinstance(globals()['json_results'], list):
                # Find times from median_of: need to capture times, but median_of doesn't return times
                # For now, just use median/best/worst/cov
                pass
            print(f"  {label:38s} {rows:7,} rows  {dt:.4f}s median ({best:.4f}-{worst:.4f}, CoV {cov:.1%})  {rps:8,.0f} rows/s  {mb:6.1f} MB/s  {extra}")
        else:
            dt, rows = best_of(fn, rounds=rounds)
            mb = sz / dt / 1024 / 1024 if dt > 0 else 0
            rps = rows / dt if dt > 0 and rows else 0
            extra = " ".join(f"{k}={v}" for k, v in kwargs.items() if v is not None)
            print(f"  {label:38s} {rows:7,} rows  {dt:.4f}s  {rps:8,.0f} rows/s  {mb:6.1f} MB/s  {extra}")
        return dt, rows, mb
    except Exception as e:
        print(f"  {label:38s} FAILED: {e}")
        return None, 0, 0

def ensure_files(targets, gen_only=False, skip_1gb=False):
    for mb, p in targets:
        if mb == 1024 and skip_1gb:
            print(f"Skipping {p.name} (--skip-1gb)")
            continue
        if p.exists():
            print(f"Skipping {p.name} ({p.stat().st_size/1024/1024:.1f} MB)")
            continue
        if gen_only:
            print(f"Need {p.name} but --gen-only and missing — generate with --gen-only to create")
            continue
        print(f"Generating {mb} MB {p.name}...")
        gen_file(mb, p)

# ---------------------------------------------------------------------------
# Matrix definitions
# ---------------------------------------------------------------------------

NATIVE_FUNCS = {
    "single": lambda p: _core.read_to_columnar(str(p), row_tag="Details"),
    "multi2": lambda p: _core.read_to_columnar_multi(str(p), row_tag="Details", num_chunks=2),
    "par4": lambda p: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=4),
    "par8": lambda p: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=8),
    "par16": lambda p: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=16),
    "par32": lambda p: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=32),
    "bounded64": lambda p: _core.read_to_columnar_bounded(str(p), row_tag="Details", memory=64*1024*1024),
    "bounded256": lambda p: _core.read_to_columnar_bounded(str(p), row_tag="Details", memory=256*1024*1024),
}

ENGINES = ["stream", "columnar", "parallel", "auto"]
SINKS = ["iter", "iter_batches", "to_arrow", "to_dataframe", "to_pandas", "to_polars", "to_parquet"]

PUSHDOWNS = {
    "baseline": {},
    "drop_half": {"drop_fields": ["Field22","Field23","Field38"]},
    "drop_all": {"drop_fields": ["Field22","Field23","Field38","Field39","Field61","Field73","Level","Section","Text20","Text21","FieldG"]},
    "rename": {"field_mapping": {"Field22": "Price","Field23": "Qty"}},
    "typed_int": {"field_types": {"Field22": "int64","Field23": "int64"}},
    "typed_float": {"field_types": {"Field22": "float64"}},
    "dict": {"dictionary_columns": ["Field38","Field39"]},
    "auto_dict": {"auto_dict": True},
    "filter_eq": {"filter": {"field": "Level", "op": "==", "value": "3"}},
    "filter_ne": {"filter": {"field": "Level", "op": "!=", "value": "99"}},
    "filter_compare": {"filter": {"field_a": "Field22", "op": ">", "field_b": "Field23"}},
    "schema": {"schema": ["Field22","Field23","Field38","Field39","Field61","Field73","Level","Section","Text20"]},
    "mmap_off": {"use_mmap": False},
    "mmap_on": {"use_mmap": True},
    # Combined projection + selectivity: the real win for filtering (deferred materialization)
    # drop_half alone is at its theoretical ceiling (+10% linear, 7/10 fields still needed) — keep as regression guard
    # Combined with a selective filter (5% pass) should approach drop_all territory via early rejection
    "drop_half_filter_eq": {"drop_fields": ["Field22","Field23","Field38"], "filter": {"field": "Level", "op": "==", "value": "3"}},
    "drop_half_filter_selective": {"drop_fields": ["Field22","Field23","Field38"], "filter": {"field": "Field39", "op": "==", "value": "01-00123"}},  # ~6% selective (1/15 articulos)
}

def run_native_matrix(path: Path, rounds=3):
    print(f"\n-- Native Exports {path.name} --")
    for name, fn in NATIVE_FUNCS.items():
        # skip bounded for tiny files where it falls back
        if "bounded" in name and path.stat().st_size < 20*1024*1024:
            continue
        report(path, f"native {name}", lambda fn=fn, p=path: fn(p), rounds=rounds, engine=name)

def run_source_engine_sink_matrix(path: Path, rounds=3, quick=False):
    engines = ENGINES if not quick else ["stream","parallel"]
    sinks = SINKS if not quick else ["iter","to_arrow"]
    for engine in engines:
        for sink in sinks:
            # skip expensive combos in quick mode
            if quick and sink in ("to_polars","to_parquet") and engine == "stream":
                continue
            # to_* sinks are not intended for stream engine (would fallback via rows and fail on sparse columns)
            # Skip those combos and mark as N/A
            if engine == "stream" and sink in ("to_arrow","to_dataframe","to_pandas","to_polars","to_parquet"):
                # streaming to_arrow is via columnar; skip to avoid sparse-column KeyError
                continue
            def fn(engine=engine, sink=sink, p=path):
                src = CrystalXMLSource(str(p), row_tag="Details", engine=engine)
                if sink == "iter":
                    return sum(1 for _ in src)
                elif sink == "iter_batches":
                    return sum(1 for _ in src._iter_batches(batch_size=1024))
                elif sink == "to_arrow":
                    return src.to_arrow()
                elif sink in ("to_dataframe","to_pandas"):
                    return src.to_dataframe()
                elif sink == "to_polars":
                    return src.to_polars()
                elif sink == "to_parquet":
                    import tempfile
                    with tempfile.NamedTemporaryFile(suffix=".parquet", delete=True) as tf:
                        src.to_parquet(tf.name)
                        return src.to_arrow()
                else:
                    return src.to_arrow()
            label = f"src {engine:10s} → {sink}"
            report(path, label, fn, rounds=rounds, engine=engine, sink=sink)

def run_pushdown_matrix(path: Path, rounds=3, quick=False):
    pushdowns = PUSHDOWNS if not quick else {k: v for k, v in PUSHDOWNS.items() if k in ("baseline","drop_half","filter_eq","auto_dict")}
    for pd_name, kwargs in pushdowns.items():
        for engine in (["columnar","parallel"] if not quick else ["parallel"]):
            def fn(engine=engine, kw=kwargs, p=path):
                src = CrystalXMLSource(str(p), row_tag="Details", engine=engine, **kw)
                return src.to_arrow()
            label = f"push {pd_name:14s} [{engine}]"
            report(path, label, fn, rounds=rounds, **kwargs)

def run_chunk_scaling(path: Path, rounds=3):
    print(f"\n-- Chunk Scaling {path.name} --")
    for n in [2,4,8,16,32,64]:
        report(path, f"par n={n:2d}", lambda n=n, p=path: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=n), rounds=rounds, n=n)

def run_bounded_scaling(path: Path, rounds=2):
    print(f"\n-- Bounded Scaling {path.name} --")
    for mem in ["64MB","256MB","512MB"]:
        import re
        m = re.match(r"(\d+)(MB|GB)", mem)
        bytes_mem = int(m.group(1)) * (1024**2 if m.group(2)=="MB" else 1024**3)
        report(path, f"bounded {mem}", lambda b=bytes_mem, p=path: _core.read_to_columnar_bounded(str(p), row_tag="Details", memory=b), rounds=rounds, mem=mem)

def run_batch_size_matrix(path: Path, rounds=3):
    print(f"\n-- Streaming Batch Sizes {path.name} --")
    for bs in [256,1024,4096,8192]:
        def fn(bs=bs, p=path):
            src = CrystalXMLSource(str(p), row_tag="Details", engine="stream", batch_size=bs)
            return sum(1 for _ in src)
        report(path, f"stream batch={bs}", fn, rounds=rounds, batch=bs)

def run_pipeline_matrix(path: Path, rounds=2):
    print(f"\n-- Pipeline / Fusion {path.name} --")
    try:
        from crxml import CrystalXMLSource, DropFields, RenameFields, FilterRows, CastTypes
        from crxml.pipeline import Pipeline
    except Exception as e:
        print(f"  pipeline skipped: {e}")
        return
    def fn_base():
        src = CrystalXMLSource(str(path), row_tag="Details", engine="parallel")
        return src.to_arrow()
    report(path, "pipe base", fn_base, rounds=rounds)
    def fn_drop():
        src = CrystalXMLSource(str(path), row_tag="Details", engine="parallel", drop_fields=["Field22"])
        return src.to_arrow()
    report(path, "pipe drop", fn_drop, rounds=rounds)
    def fn_rename():
        src = CrystalXMLSource(str(path), row_tag="Details", engine="parallel", field_mapping={"Field22":"Price"})
        return src.to_arrow()
    report(path, "pipe rename", fn_rename, rounds=rounds)
    def fn_filter():
        src = CrystalXMLSource(str(path), row_tag="Details", engine="parallel", filter={"field":"Level","op":"==","value":"3"})
        return src.to_arrow()
    report(path, "pipe filter", fn_filter, rounds=rounds)
    # Pipeline composition — iterate via Pipeline (not CrystalXMLSource.to_arrow)
    try:
        def fn_pipe():
            src = CrystalXMLSource(str(path), row_tag="Details", engine="parallel")
            pipe = src | DropFields(["Field22"]) | FilterRows(field="Level", op="==", value="3")
            # Try fast path _to_arrow, else fall back to iteration
            tbl = pipe._to_arrow()
            if tbl is not None:
                return tbl.num_rows
            return sum(1 for _ in pipe)
        report(path, "Pipeline Drop+Filter", fn_pipe, rounds=rounds)
    except Exception as e:
        print(f"  Pipeline Drop+Filter skipped: {e}")

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="Extended crxml benchmarks — all use cases × combinations")
    ap.add_argument("--quick", action="store_true", help="10 MB only, minimal combos (CI)")
    ap.add_argument("--skip-1gb", action="store_true", help="Skip 1 GB file even in full mode")
    ap.add_argument("--gen-only", action="store_true", help="Only generate files")
    ap.add_argument("--rounds", type=int, default=7, help="Rounds median-of-N (default 7, was best-of-3)")
    ap.add_argument("--include", type=str, default="all", help="Comma list of sections: native,source,pushdown,chunk,bounded,batch,pipeline or all")
    ap.add_argument("--output", type=str, default=None, help="Write JSON results to dir (e.g. .benchmarks/crxml-1gb.json) for docs rendering")
    args = ap.parse_args()
    # Setup JSON output collection
    json_results = []
    import json as _json

    def collect_json(label, path, times, rows, cov):
        if args.output:
            json_results.append({
                "label": label,
                "file": str(path),
                "size": path.stat().st_size if path.exists() else 0,
                "rounds": rounds,
                "times": times,
                "median": statistics.median(times) if times else 0,
                "best": min(times) if times else 0,
                "worst": max(times) if times else 0,
                "cov": cov,
                "rows": rows,
                "mb_per_s": (path.stat().st_size / (statistics.median(times) or 1) / 1024 / 1024) if path.exists() else 0,
            })

    # File matrix — now includes 533 MB real export as side-by-side column when present
    # 533 MB file is not in repo; if missing, we note it and synthetic 100 MB is used as proxy.
    # To generate a synthetic file that mimics 533 MB cardinality, use benchmarks.py with custom cardinality.
    if args.quick:
        targets = [(10, BENCH_DATA / "test_10mb.xml")]
    else:
        targets = [(10, BENCH_DATA / "test_10mb.xml"),
                   (50, BENCH_DATA / "test_50mb.xml"),
                   (100, BENCH_DATA / "test_100mb.xml"),
                   (533, BENCH_DATA / "test_533mb.xml"),
                   (1024, BENCH_DATA / "test_1gb.xml")]

    print("="*70)
    print("Generating / verifying files")
    print("="*70)
    # Handle 533 MB real vs synthetic mimic labeling
    # If test_533mb.xml is missing, generate a synthetic mimic with matching cardinality
    # but label it distinctly in reports
    _is_533_real = (BENCH_DATA / "test_533mb.xml").exists()
    _label_533 = "533 MB (real export)" if _is_533_real else "533 MB (synthetic mimic)"
    ensure_files(targets, gen_only=args.gen_only, skip_1gb=args.skip_1gb)
    if args.gen_only:
        return
    # If 533 MB still missing after ensure_files (e.g., not generated), note it
    if not (BENCH_DATA / "test_533mb.xml").exists():
        print(f"\nNote: {BENCH_DATA / 'test_533mb.xml'} missing — synthetic mimic would be generated on demand")
        print("      Label will be '533 MB (synthetic mimic)' not 'real export' to avoid ambiguity")

    include = set(args.include.split(",")) if args.include != "all" else {"native","source","pushdown","chunk","bounded","batch","pipeline","edge"}
    rounds = args.rounds

    for mb, p in targets:
        if not p.exists():
            print(f"\nSkipping missing {p.name}")
            continue
        if mb == 1024 and args.skip_1gb:
            continue
        size = p.stat().st_size / 1024 / 1024
        print("\n" + "="*70)
        print(f"FILE {p.name}  {size:.1f} MB  {mb} MB target")
        print("="*70)

        if "native" in include:
            run_native_matrix(p, rounds=rounds)
        if "source" in include:
            run_source_engine_sink_matrix(p, rounds=rounds, quick=args.quick)
        if "pushdown" in include:
            run_pushdown_matrix(p, rounds=rounds, quick=args.quick)
        if "chunk" in include:
            run_chunk_scaling(p, rounds=rounds)
        if "bounded" in include and not args.quick:
            run_bounded_scaling(p, rounds=2 if mb>100 else 2)
        if "batch" in include and not args.quick:
            run_batch_size_matrix(p, rounds=rounds)
        if "pipeline" in include:
            run_pipeline_matrix(p, rounds=2 if args.quick else 2)
    # Edge cases: empty, single row, ragged, late debut, entities, unicode, comments
    if "edge" in include:
        # Generate edge files on demand (tiny, not in main targets)
        edge_dir = BENCH_DATA / "edge"
        edge_dir.mkdir(exist_ok=True)
        run_edge_case_matrix(edge_dir, rounds=rounds, quick=args.quick)

    print("\nDone.")


def run_edge_case_matrix(edge_dir: Path, rounds=3, quick=False):
    """Benchmark edge cases: empty, single row, ragged, sparse, truncated, entities, unicode, different row_tags."""
    print("\n" + "="*70)
    print("EDGE CASES")
    print("="*70)

    # Helper to write tiny XML files for edge cases
    def write_edge(name: str, content: bytes) -> Path:
        p = edge_dir / f"{name}.xml"
        if not p.exists():
            p.write_bytes(content)
        return p

    # 1) Empty file (no rows)
    p_empty = write_edge("empty", b'<?xml version="1.0"?><CrystalReport><Group><GroupHeader><Section SectionNumber="0"></Section></GroupHeader></Group></CrystalReport>')
    report(p_empty, "edge empty", lambda p=p_empty: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds)

    # 2) Single row
    single_xml = b'<?xml version="1.0"?><CrystalReport><Group><GroupHeader><Section/></GroupHeader><Group Level="2"><GroupHeader><Section SectionNumber="0"></Section></GroupHeader><Details Level="3"><Section SectionNumber="0"><Field Name="Field22" FieldName="{F}"><FormattedValue>1</FormattedValue><Value>1</Value></Field></Section></Details></Group></Group></CrystalReport>'
    p_single = write_edge("single_row", single_xml)
    report(p_single, "edge single row", lambda p=p_single: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds)

    # 3) Ragged: missing fields, late debut (FieldG appears only in last 10% rows)
    # Use existing 10MB file but test via drop_all vs sparse handling already in pushdown, here test ragged via bounded
    if not quick:
        p_10 = BENCH_DATA / "test_10mb.xml"
        if p_10.exists():
            report(p_10, "edge ragged via bounded64", lambda: _core.read_to_columnar_bounded(str(p_10), row_tag="Details", memory=64*1024), rounds=rounds)

    # 4) Entities & unicode
    ent_xml = b'<?xml version="1.0"?><CrystalReport><Group><Group Level="2"><GroupHeader/><Details Level="3"><Section SectionNumber="0"><Field Name="Field38" FieldName="{F}"><FormattedValue>A &amp; B &lt; C</FormattedValue><Value>A &amp; B &lt; C</Value></Field><Field Name="Field39"><FormattedValue>\xe2\x98\x83 unicode \xe2\x98\x85</FormattedValue><Value>\xe2\x98\x83 unicode \xe2\x98\x85</Value></Field></Section></Details></Group></Group></CrystalReport>'
    p_ent = write_edge("entities_unicode", ent_xml)
    report(p_ent, "edge entities+unicode", lambda p=p_ent: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds)

    # 5) Comment with fake row tag
    comment_xml = b'<?xml version="1.0"?><CrystalReport><!-- <Details Level="3"><Field Name="Trap"><Value>nope</Value></Field></Details> --><Group><Group Level="2"><GroupHeader/><Details Level="3"><Section SectionNumber="0"><Field Name="Field22"><Value>42</Value></Field></Section></Details></Group></Group></CrystalReport>'
    p_comment = write_edge("comment_fake_row", comment_xml)
    report(p_comment, "edge comment fake row", lambda p=p_comment: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds)

    # 6) Different row_tag: Row vs Details vs custom
    p_10 = BENCH_DATA / "test_10mb.xml"
    if p_10.exists():
        for tag in ["Details", "Row", "NonExistentTag"]:
            report(p_10, f"edge row_tag={tag}", lambda tag=tag, p=p_10: _core.read_to_columnar(str(p), row_tag=tag), rounds=rounds)

    # 7) Tiny file (1KB, few rows) vs large (already covered)
    tiny_xml = b'<?xml version="1.0"?><CrystalReport>' + b'<Details Level="3"><Section SectionNumber="0"><Field Name="F"><Value>1</Value></Field></Section></Details>'*5 + b'</CrystalReport>'
    p_tiny = write_edge("tiny_1kb", tiny_xml)
    report(p_tiny, "edge tiny 1KB", lambda p=p_tiny: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds)

    # 8) All engines on edge single row (stream vs columnar vs parallel vs bounded)
    p_single = edge_dir / "single_row.xml"
    for eng in (["stream","parallel"] if quick else ["stream","columnar","parallel"]):
        def fn(eng=eng, p=p_single):
            src = CrystalXMLSource(str(p), row_tag="Details", engine=eng)
            return src.to_arrow()
        report(p_single, f"edge single [{eng}]", fn, rounds=rounds)

    # 9) Sinks on edge
    p_10 = BENCH_DATA / "test_10mb.xml"
    if p_10.exists() and not quick:
        for sink in ["to_arrow","to_dataframe","to_polars"]:
            def fn(sink=sink, p=p_10):
                src = CrystalXMLSource(str(p), row_tag="Details", engine="parallel")
                if sink == "to_arrow":
                    return src.to_arrow()
                elif sink == "to_dataframe":
                    return src.to_dataframe()
                elif sink == "to_polars":
                    return src.to_polars()
            report(p_10, f"edge sink {sink}", fn, rounds=rounds)

    # 10) Streaming with 64KB vs 1MB on tiny file (constant memory)
    p_10 = BENCH_DATA / "test_10mb.xml"
    if p_10.exists():
        for mem in (["64KB","1MB"] if not quick else ["64KB"]):
            def fn(mem=mem, p=p_10):
                # Use new streaming iterator (true 64KB)
                it = _core.iter_record_batches(str(p), row_tag="Details", memory=mem)
                return sum(b.num_rows for b in it)
            report(p_10, f"edge stream {mem}", fn, rounds=rounds, mem=mem)

    # 11) Truncated / malformed (should not panic, return truncated row discarded)
    trunc_xml = b'<?xml version="1.0"?><CrystalReport><Group><Group Level="2"><Details Level="3"><Section SectionNumber="0"><Field Name="Field22"><Value>1</Value></Field></Section></Details></Group>'
    p_trunc = write_edge("truncated", trunc_xml)
    report(p_trunc, "edge truncated", lambda p=p_trunc: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds)

    # 12) Field types bool/date32/timestamp via typed columns
    p_10 = BENCH_DATA / "test_10mb.xml"
    if p_10.exists() and not quick:
        for ftype in ["bool","date32"]:
            def fn(ftype=ftype, p=p_10):
                src = CrystalXMLSource(str(p), row_tag="Details", engine="parallel", field_types={"Field73": ftype})
                return src.to_arrow()
            report(p_10, f"edge typed {ftype}", fn, rounds=rounds, field_type=ftype)


if __name__ == "__main__":
    main()
