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
import json as _json
import os
import statistics
import sys
import tempfile
import time
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Any, Callable, Optional

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
# Build provenance: verify the .so matches HEAD
# ---------------------------------------------------------------------------
import subprocess as _subprocess
def _verify_build_sha(allow_dirty=False):
    """Assert that the installed extension was built from the current HEAD.

    Prevents benchmarks from silently measuring stale Rust code — the exact
    failure mode that invalidated all production numbers in the Aug 28 thread.

    build.rs appends "-dirty" to the SHA when uncommitted edits exist.
    A dirty build means the binary doesn't reflect working-tree changes.
    Pass allow_dirty=True (via --allow-dirty) to bypass for quick iteration.
    """
    try:
        build_sha = getattr(_core, '__build_sha__', None)
        if build_sha is None:
            print("  WARNING: extension has no __build_sha__ — cannot verify provenance")
            return
        head_sha = _subprocess.check_output(
            ["git", "rev-parse", "HEAD"],
            cwd=str(Path(__file__).resolve().parent.parent)
        ).decode().strip()
        is_dirty = build_sha.endswith("-dirty")
        base_sha = build_sha.removesuffix("-dirty")
        if base_sha != head_sha:
            raise SystemExit(
                f"FATAL: extension built at {base_sha[:9]} but HEAD is {head_sha[:9]}\n"
                f"  Run: cargo build --release (or maturin develop --release)\n"
                f"  Then copy .so to site-packages and re-run benchmarks."
            )
        if is_dirty and not allow_dirty:
            raise SystemExit(
                f"FATAL: extension built from {base_sha[:9]} but working tree is dirty\n"
                f"  Uncommitted changes are not compiled into the binary.\n"
                f"  Either commit or pass --allow-dirty to benchmark anyway."
            )
        label = f"{base_sha[:9]}" + (" (dirty)" if is_dirty else "")
        print(f"  Build SHA: {label} (matches HEAD)")
    except FileNotFoundError:
        print("  WARNING: git not found, skipping build-SHA check")
    except SystemExit:
        raise
    except Exception as e:
        print(f"  WARNING: build-SHA check failed: {e}")

# ---------------------------------------------------------------------------
# Environment setup for reproducibility
# ---------------------------------------------------------------------------
def _parse_thp_active(raw: str) -> str:
    """Extract the active THP defrag mode from sysfs.

    sysfs format: "always defer defer+madvise [madvise] never"
    The brackets [mode] mark the currently active setting.
    """
    import re
    m = re.search(r'\[(\w[\w+]*)\]', raw)
    return m.group(1) if m else raw

def setup_thp_madvise():
    """Report the active transparent hugepage defrag mode.

    sysfs format lists all modes with the active one in brackets:
        always defer defer+madvise [madvise] never

    If not already madvise, attempt to switch (requires root).
    Returns the effective mode after the attempt (for JSON provenance).
    """
    thp_path = "/sys/kernel/mm/transparent_hugepage/defrag"
    try:
        if not os.path.exists(thp_path):
            return "no-thp"
        raw = open(thp_path).read().strip()
        active = _parse_thp_active(raw)
        if active != "madvise":
            try:
                with open(thp_path, "w") as f:
                    f.write("madvise\n")
            except PermissionError:
                pass
            # Read back to verify
            raw_after = open(thp_path).read().strip()
            active = _parse_thp_active(raw_after)
        print(f"  THP defrag: {active} (from: {raw})")
        return active
    except Exception as e:
        print(f"  THP defrag: error {e}")
        return "error"

# ---------------------------------------------------------------------------
# Declarative benchmark config — serializable to subprocess
# ---------------------------------------------------------------------------
@dataclass
class BenchConfig:
    """Declarative benchmark config — all fields JSON-serializable.

    Used to dispatch benchmarks in subprocesses without serializing lambdas.
    Each config type reconstructs the benchmark function from its params.
    """
    type: str          # "native" | "source_sink" | "pushdown" | "chunk" | "bounded" | "batch" | "par_stream" | "pipeline"
    file: str          # absolute path to XML file
    label: str         # display label
    engine: str = ""
    sink: str = ""
    n: int = 0         # chunks for native/chunk
    memory: str = ""   # for bounded/par_stream (e.g. "64MB")
    mem_bytes: int = 0 # for bounded (raw bytes)
    batch: int = 0     # for batch
    threads: int = 0   # for par_stream
    pushdown: dict = field(default_factory=dict)  # for pushdown kwargs
    extra: dict = field(default_factory=dict)      # catch-all (row_tag, etc.)

def _config_to_fn(config: BenchConfig) -> Callable:
    """Reconstruct a benchmark function from a BenchConfig.

    This is the in-process fallback when --isolate is not used.
    """
    p = Path(config.file)
    row_tag = config.extra.get("row_tag", "Details")

    if config.type == "native":
        return lambda: NATIVE_FUNCS[config.engine](p)
    elif config.type == "source_sink":
        engine, sink = config.engine, config.sink
        def _fn():
            src = CrystalXMLSource(str(p), row_tag=row_tag, engine=engine)
            if sink == "iter":
                return sum(1 for _ in src)
            elif sink == "iter_batches":
                return sum(1 for _ in src._iter_batches(batch_size=1024))
            elif sink == "to_arrow":
                return src.to_arrow()
            elif sink in ("to_dataframe", "to_pandas"):
                return src.to_dataframe()
            elif sink == "to_polars":
                return src.to_polars()
            elif sink == "to_parquet":
                with tempfile.NamedTemporaryFile(suffix=".parquet", delete=True) as tf:
                    src.to_parquet(tf.name)
                    return src.to_arrow()
            else:
                return src.to_arrow()
        return _fn
    elif config.type == "pushdown":
        engine = config.engine
        kw = config.pushdown
        def _fn():
            src = CrystalXMLSource(str(p), row_tag=row_tag, engine=engine, **kw)
            return src.to_arrow()
        return _fn
    elif config.type == "chunk":
        n = config.n
        def _fn():
            return _core.read_to_columnar_par(str(p), row_tag=row_tag, num_chunks=n)
        return _fn
    elif config.type == "bounded":
        mem = config.mem_bytes
        def _fn():
            return _core.read_to_columnar_bounded(str(p), row_tag=row_tag, memory=mem)
        return _fn
    elif config.type == "batch":
        bs = config.batch
        def _fn():
            src = CrystalXMLSource(str(p), row_tag=row_tag, engine="stream", batch_size=bs)
            return sum(1 for _ in src)
        return _fn
    elif config.type == "par_stream":
        mem = config.memory
        t = config.threads
        def _fn():
            src = CrystalXMLSource(str(p), row_tag=row_tag, engine="parallel",
                                   memory=mem, threads=t)
            return sum(len(b) for b in src.iter_record_batches(memory=mem, threads=t))
        return _fn
    elif config.type == "pipeline":
        pipe_name = config.extra.get("pipe_name", "")
        def _fn():
            src = CrystalXMLSource(str(p), row_tag=row_tag, engine="parallel")
            if pipe_name == "base":
                return src.to_arrow()
            elif pipe_name == "drop":
                src = CrystalXMLSource(str(p), row_tag=row_tag, engine="parallel",
                                       drop_fields=["Field22"])
                return src.to_arrow()
            elif pipe_name == "rename":
                src = CrystalXMLSource(str(p), row_tag=row_tag, engine="parallel",
                                       field_mapping={"Field22": "Price"})
                return src.to_arrow()
            elif pipe_name == "filter":
                src = CrystalXMLSource(str(p), row_tag=row_tag, engine="parallel",
                                       filter={"field": "Level", "op": "==", "value": "3"})
                return src.to_arrow()
            elif pipe_name == "Drop+Filter":
                from crxml import DropFields, FilterRows
                src = CrystalXMLSource(str(p), row_tag=row_tag, engine="parallel")
                pipe = src | DropFields(["Field22"]) | FilterRows(field="Level", op="==", value="3")
                tbl = pipe._to_arrow()
                if tbl is not None:
                    return tbl.num_rows
                return sum(1 for _ in pipe)
            return src.to_arrow()
        return _fn
    else:
        raise ValueError(f"Unknown config type: {config.type}")

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _extract_rows(res) -> int:
    """Extract row count from a benchmark result."""
    if isinstance(res, int):
        return res
    if hasattr(res, "num_rows"):
        return res.num_rows
    if hasattr(res, "__len__"):
        try:
            return len(res)
        except Exception:
            return 0
    return 0

def best_of(fn, rounds=3):
    best = float("inf")
    rows = 0
    for _ in range(rounds):
        t0 = time.perf_counter()
        res = fn()
        dt = time.perf_counter() - t0
        rows = _extract_rows(res)
        best = min(best, dt)
    return best, rows, rounds

def median_of(fn, rounds=7):
    """Adaptive median: keep sampling until 1.31*CoV <= 5%, capped at 31.

    Returns (median, best, worst, cov, rows, n, times) where n is the actual
    sample count and times is the raw sorted list.  n is critical for
    transparency: hitting the cap of 31 means the noise floor was not achieved
    and the cell is untrustworthy.
    """
    times = []
    rows = 0
    target_rounds = rounds
    while len(times) < target_rounds:
        # Warmup + tiny-file batching: probe once, discard, then batch if <50 ms
        if len(times) == 0:
            t_probe0 = time.perf_counter()
            res = fn()
            t_probe = time.perf_counter() - t_probe0
            rows = _extract_rows(res)
            if t_probe < 0.05:  # 50 ms threshold — batch 20× inside one timed region
                t0 = time.perf_counter()
                for _ in range(20):
                    fn()
                dt = (time.perf_counter() - t0) / 20
                times.append(dt)
                continue
            else:
                # Discard warmup, start fresh collection below
                pass
        t0 = time.perf_counter()
        res = fn()
        dt = time.perf_counter() - t0
        times.append(dt)
        rows = _extract_rows(res)
        if len(times) >= 7:
            mean = sum(times) / len(times)
            stdev = statistics.pstdev(times) if len(times) > 1 else 0
            cov = stdev / mean if mean else 0
            floor = 1.31 * cov
            if floor <= 0.05 or len(times) >= 31:
                break
    times.sort()
    median = times[len(times) // 2]
    best = min(times)
    worst = max(times)
    mean = sum(times) / len(times)
    stdev = statistics.pstdev(times) if len(times) > 1 else 0
    cov = stdev / mean if mean else 0
    n = len(times)
    return median, best, worst, cov, rows, n, times

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

# ---------------------------------------------------------------------------
# Subprocess isolation
# ---------------------------------------------------------------------------

# Module-level globals set by main() for subprocess access
_isolate = False
_taskset = None
_warm_cache = False
_isolate_min_size = 50  # MB: skip subprocess isolation for smaller files

def _run_subprocess_isolated(config: BenchConfig, rounds: int, file_size: int) -> dict:
    """Run a benchmark in a fresh subprocess.

    Serializes the config as JSON, spawns a child process that reconstructs
    the function and runs median_of.  Returns dict with timing stats or
    error info.
    """
    import subprocess

    config_json = _json.dumps(asdict(config))

    # Build the subprocess command
    # Add crxml dir (for crxml import) and benchmarks dir (for bench_extended import)
    # Use direct import since benchmarks/ may not be a package.
    # Config is passed via stdin as JSON to avoid Python literal issues (true vs True).
    cmd_parts = [sys.executable, "-c", f"""
import sys, time, statistics, json
from pathlib import Path

# Ensure imports work: crxml dir first, then benchmarks dir
sys.path.insert(0, {str(HERE.parent)!r})
sys.path.insert(0, {str(HERE)!r})

from bench_extended import (
    BenchConfig, _config_to_fn, median_of, _extract_rows, BENCH_DATA
)

config = BenchConfig(**json.loads(sys.stdin.read()))
fn = _config_to_fn(config)
file_size = {file_size}

# Evict page cache + warm pass if requested
if {repr(_warm_cache)}:
    try:
        fd = os.open(config.file, os.O_RDONLY)
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
        os.close(fd)
    except Exception:
        pass
    fn()

# Run the benchmark
result = median_of(fn, rounds={rounds})

median, best, worst, cov, rows, n, times = result

# Compute derived values
mb = file_size / median / 1024 / 1024 if median > 0 else 0
rps = rows / median if median > 0 and rows else 0

# Capture peak RSS (VmHWM) from /proc/self/status
rss_kb = 0
try:
    for line in open('/proc/self/status'):
        if line.startswith('VmHWM:'):
            rss_kb = int(line.split()[1])
            break
except Exception:
    pass

output = {{
    "median": median,
    "best": best,
    "worst": worst,
    "cov": cov,
    "rows": rows,
    "n": n,
    "times": times,
    "mb_per_s": mb,
    "rows_per_s": rps,
    "rss_mb": rss_kb / 1024,
}}
print(json.dumps(output))
"""]

    # Add taskset pinning if requested
    if _taskset:
        cmd_parts = ["taskset", "-c", _taskset] + cmd_parts

    try:
        r = subprocess.run(
            cmd_parts,
            input=config_json,
            capture_output=True,
            text=True,
            timeout=max(300, rounds * 60),  # generous timeout
        )
        if r.returncode != 0:
            return {"error": f"subprocess exited {r.returncode}: {r.stderr[:500]}"}
        # Parse JSON from last line of stdout (skip any warning lines)
        for line in reversed(r.stdout.strip().splitlines()):
            line = line.strip()
            if line.startswith("{"):
                return _json.loads(line)
        return {"error": f"no JSON in stdout: {r.stdout[:300]}"}
    except subprocess.TimeoutExpired:
        return {"error": "subprocess timed out"}
    except Exception as e:
        return {"error": str(e)}

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

def report(path: Path, label_or_config, fn=None, rounds=3, collect: Optional[Callable] = None, **kwargs):
    """Run a benchmark and print results.

    Accepts either:
      - (path, label, fn, rounds, collect=..., **kwargs)  — function-based
      - (path, config, rounds=..., collect=...)            — BenchConfig-based

    collect(label, path, times, rows, cov) is called when provided, enabling
    JSON output without globals hacks.
    """
    sz = path.stat().st_size

    # Resolve label and function from config or explicit args
    if isinstance(label_or_config, BenchConfig):
        config = label_or_config
        label = config.label
        if _isolate and sz >= _isolate_min_size * 1024 * 1024:
            result = _run_subprocess_isolated(config, rounds, sz)
            if "error" in result:
                print(f"  {label:38s} FAILED: {result['error']}")
                return None, 0, 0
            dt = result["median"]
            best = result["best"]
            worst = result["worst"]
            cov = result["cov"]
            rows = result["rows"]
            n = result["n"]
            times = result["times"]
            mb = result["mb_per_s"]
            rps = result["rows_per_s"]
            rss = result.get("rss_mb", 0)
            extra = " ".join(f"{k}={v}" for k, v in kwargs.items() if v is not None)
            print(f"  {label:38s} {rows:7,} rows  {dt:.4f}s median ({best:.4f}-{worst:.4f}, CoV {cov:.1%}, n={n})  {rps:8,.0f} rows/s  {mb:6.1f} MB/s  {rss:5.1f} MB rss  {extra}")
            if collect is not None:
                collect(label, path, times, rows, cov)
            return dt, rows, mb
        else:
            fn = _config_to_fn(config)
    else:
        label = label_or_config

    if fn is None:
        print(f"  {label:38s} FAILED: no function provided")
        return None, 0, 0

    try:
        if rounds >= 7:
            median, best, worst, cov, rows, n, times = median_of(fn, rounds=rounds)
            dt = median
            mb = sz / dt / 1024 / 1024 if dt > 0 else 0
            rps = rows / dt if dt > 0 and rows else 0
            extra = " ".join(f"{k}={v}" for k, v in kwargs.items() if v is not None)
            print(f"  {label:38s} {rows:7,} rows  {dt:.4f}s median ({best:.4f}-{worst:.4f}, CoV {cov:.1%}, n={n})  {rps:8,.0f} rows/s  {mb:6.1f} MB/s  {extra}")
            if collect is not None:
                collect(label, path, times, rows, cov)
        else:
            dt, rows, n = best_of(fn, rounds=rounds)
            mb = sz / dt / 1024 / 1024 if dt > 0 else 0
            rps = rows / dt if dt > 0 and rows else 0
            extra = " ".join(f"{k}={v}" for k, v in kwargs.items() if v is not None)
            print(f"  {label:38s} {rows:7,} rows  {dt:.4f}s (n={n})  {rps:8,.0f} rows/s  {mb:6.1f} MB/s  {extra}")
            if collect is not None:
                # For best_of, fabricate a single-element times list
                collect(label, path, [dt], rows, 0.0)
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

def _auto_chunks(path):
    """Replicate the auto chunk rule from CrystalXMLSource."""
    import multiprocessing
    threads = multiprocessing.cpu_count()
    file_bytes = os.path.getsize(str(path))
    return max(threads, min(16 * threads, file_bytes // (4 * 1024 * 1024)))

NATIVE_FUNCS = {
    "single": lambda p: _core.read_to_columnar(str(p), row_tag="Details"),
    "multi2": lambda p: _core.read_to_columnar_multi(str(p), row_tag="Details", num_chunks=2),
    "par4": lambda p: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=4),
    "par8": lambda p: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=8),
    "par16": lambda p: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=16),
    "par32": lambda p: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=32),
    "par_auto": lambda p: _core.read_to_columnar_par(str(p), row_tag="Details", num_chunks=_auto_chunks(p)),
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

def run_native_matrix(path: Path, rounds=3, only_config=None, collect=None):
    print(f"\n-- Native Exports {path.name} --")
    for name in NATIVE_FUNCS:
        # skip bounded for tiny files where it falls back
        if "bounded" in name and path.stat().st_size < 20*1024*1024:
            continue
        if only_config and name != only_config:
            continue
        config = BenchConfig(
            type="native", file=str(path), label=f"native {name}",
            engine=name, extra={"row_tag": "Details"},
        )
        report(path, config, rounds=rounds, collect=collect, engine=name)

def run_source_engine_sink_matrix(path: Path, rounds=3, quick=False, collect=None):
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
            label = f"src {engine:10s} → {sink}"
            config = BenchConfig(
                type="source_sink", file=str(path), label=label,
                engine=engine, sink=sink, extra={"row_tag": "Details"},
            )
            report(path, config, rounds=rounds, collect=collect, engine=engine, sink=sink)

def run_pushdown_matrix(path: Path, rounds=3, quick=False, collect=None):
    pushdowns = PUSHDOWNS if not quick else {k: v for k, v in PUSHDOWNS.items() if k in ("baseline","drop_half","filter_eq","auto_dict")}
    for pd_name, kwargs in pushdowns.items():
        for engine in (["columnar","parallel"] if not quick else ["parallel"]):
            label = f"push {pd_name:14s} [{engine}]"
            config = BenchConfig(
                type="pushdown", file=str(path), label=label,
                engine=engine, pushdown=kwargs, extra={"row_tag": "Details"},
            )
            report(path, config, rounds=rounds, collect=collect, **kwargs)

def run_chunk_scaling(path: Path, rounds=3, collect=None):
    print(f"\n-- Chunk Scaling {path.name} --")
    for n in [2,4,8,16,32,64,128,256]:
        config = BenchConfig(
            type="chunk", file=str(path), label=f"par n={n:3d}",
            n=n, extra={"row_tag": "Details"},
        )
        report(path, config, rounds=rounds, collect=collect, n=n)

def run_bounded_scaling(path: Path, rounds=3, collect=None):
    print(f"\n-- Bounded Scaling {path.name} --")
    import re as _re
    for mem in ["64MB","256MB","512MB"]:
        m = _re.match(r"(\d+)(MB|GB)", mem)
        bytes_mem = int(m.group(1)) * (1024**2 if m.group(2)=="MB" else 1024**3)
        config = BenchConfig(
            type="bounded", file=str(path), label=f"bounded {mem}",
            mem_bytes=bytes_mem, extra={"row_tag": "Details"},
        )
        report(path, config, rounds=rounds, collect=collect, mem=mem)

def run_batch_size_matrix(path: Path, rounds=3, collect=None):
    print(f"\n-- Streaming Batch Sizes {path.name} --")
    for bs in [256,1024,4096,8192]:
        config = BenchConfig(
            type="batch", file=str(path), label=f"stream batch={bs}",
            batch=bs, extra={"row_tag": "Details"},
        )
        report(path, config, rounds=rounds, collect=collect, batch=bs)

def run_par_stream_matrix(path: Path, rounds=3, collect=None):
    print(f"\n-- Parallel Streaming {path.name} --")
    file_mb = path.stat().st_size / (1024 * 1024)
    print(f"  File: {path.name} ({file_mb:.0f} MB)")
    # Memory scaling: bounded at different memory budgets
    for mem_mb in [32, 64, 128, 256]:
        config = BenchConfig(
            type="bounded", file=str(path), label=f"bounded {mem_mb:3d}MB",
            mem_bytes=mem_mb * 1024 * 1024, extra={"row_tag": "Details"},
        )
        report(path, config, rounds=rounds, collect=collect, mem=f"{mem_mb}MB")
    # iter_record_batches: streaming with different memory budgets
    for mem_mb in [16, 64, 256]:
        config = BenchConfig(
            type="par_stream", file=str(path), label=f"iter-batch {mem_mb:3d}MB",
            memory=f"{mem_mb}MB", threads=0, extra={"row_tag": "Details"},
        )
        report(path, config, rounds=rounds, collect=collect, mem=f"{mem_mb}MB")
    # iter_record_batches with thread scaling at 64MB
    for threads in [4, 8, 16]:
        config = BenchConfig(
            type="par_stream", file=str(path), label=f"iter-batch 64MB t={threads:2d}",
            memory="64MB", threads=threads, extra={"row_tag": "Details"},
        )
        report(path, config, rounds=rounds, collect=collect, threads=threads)

def run_pipeline_matrix(path: Path, rounds=2, collect=None):
    print(f"\n-- Pipeline / Fusion {path.name} --")
    try:
        from crxml import DropFields, RenameFields, FilterRows, CastTypes
        from crxml.pipeline import Pipeline
    except Exception as e:
        print(f"  pipeline skipped: {e}")
        return
    for pipe_name in ["base", "drop", "rename", "filter", "Drop+Filter"]:
        config = BenchConfig(
            type="pipeline", file=str(path), label=f"pipe {pipe_name}",
            extra={"row_tag": "Details", "pipe_name": pipe_name},
        )
        report(path, config, rounds=rounds, collect=collect)

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    ap = argparse.ArgumentParser(description="Extended crxml benchmarks — all use cases × combinations")
    ap.add_argument("--quick", action="store_true", help="10 MB only, minimal combos (CI)")
    ap.add_argument("--skip-1gb", action="store_true", help="Skip 1 GB file even in full mode")
    ap.add_argument("--gen-only", action="store_true", help="Only generate files")
    ap.add_argument("--rounds", type=int, default=7, help="Rounds median-of-N (default 7, was best-of-3)")
    ap.add_argument("--include", type=str, default="all", help="Comma list of sections: native,source,pushdown,chunk,bounded,batch,par_stream,pipeline,edge or all")
    ap.add_argument("--output", type=str, default=None, help="Write JSON results to dir (e.g. .benchmarks/crxml-1gb.json) for docs rendering")
    ap.add_argument("--only-config", type=str, default=None,
                    help="Run only this native config (e.g. single, par16). For per-config subprocess isolation.")
    ap.add_argument("--file", type=str, default=None,
                    help="Run only this file (e.g. test_1gb.xml). Skip other file sizes.")
    ap.add_argument("--allow-dirty", action="store_true",
                    help="Allow benchmarking with a dirty working tree (uncommitted changes not in .so)")
    ap.add_argument("--isolate", action="store_true",
                    help="Run each config in a fresh subprocess (eliminates cross-config contamination)")
    ap.add_argument("--taskset", type=str, default=None,
                    help="Pin to CPU list via taskset, e.g. '0-15' for all cores")
    ap.add_argument("--warm-cache", action="store_true",
                    help="Run a warm pass before timed measurements (evicts cold-start overhead)")
    ap.add_argument("--isolate-min-size", type=int, default=50,
                    help="Skip subprocess isolation for files smaller than N MB (default 50)")
    args = ap.parse_args()
    # Verify extension matches HEAD before any measurements
    _verify_build_sha(allow_dirty=args.allow_dirty)
    # Setup THP defrag before any benchmarks — record for provenance
    _thp_value = setup_thp_madvise()

    # Set module-level globals for subprocess access
    global _isolate, _taskset, _warm_cache, _isolate_min_size
    _isolate = args.isolate
    _taskset = args.taskset
    _warm_cache = args.warm_cache
    _isolate_min_size = args.isolate_min_size

    # Setup JSON output collection with provenance
    json_results = []
    import subprocess
    try:
        _commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=str(Path(__file__).resolve().parent.parent)).decode().strip()
    except Exception:
        _commit = "unknown"
    try:
        _dirty = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=str(Path(__file__).resolve().parent.parent)).decode().strip())
    except Exception:
        _dirty = False

    def collect_json(label, path, times, rows, cov):
        if args.output:
            # Capture peak RSS for in-process path (subprocess path reports VmHWM separately)
            try:
                import resource
                rss_kb = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
                rss_mb = rss_kb / 1024  # Linux: ru_maxrss is in KB
            except Exception:
                rss_mb = 0
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
                "rss_mb": rss_mb,
                "commit": _commit,
                "dirty": _dirty,
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

    include = set(args.include.split(",")) if args.include != "all" else {"native","source","pushdown","chunk","bounded","batch","par_stream","pipeline","edge"}
    rounds = args.rounds

    for mb, p in targets:
        if not p.exists():
            print(f"\nSkipping missing {p.name}")
            continue
        if mb == 1024 and args.skip_1gb:
            continue
        if args.file and p.name != args.file:
            continue
        size = p.stat().st_size / 1024 / 1024
        print("\n" + "="*70)
        print(f"FILE {p.name}  {size:.1f} MB  {mb} MB target")
        print("="*70)

        if "native" in include:
            run_native_matrix(p, rounds=rounds, only_config=args.only_config, collect=collect_json)
        if "source" in include:
            run_source_engine_sink_matrix(p, rounds=rounds, quick=args.quick, collect=collect_json)
        if "pushdown" in include:
            run_pushdown_matrix(p, rounds=rounds, quick=args.quick, collect=collect_json)
        if "chunk" in include:
            run_chunk_scaling(p, rounds=rounds, collect=collect_json)
        if "bounded" in include and not args.quick:
            run_bounded_scaling(p, rounds=rounds, collect=collect_json)
        if "batch" in include and not args.quick:
            run_batch_size_matrix(p, rounds=rounds, collect=collect_json)
        if "par_stream" in include and not args.quick:
            run_par_stream_matrix(p, rounds=rounds, collect=collect_json)
        if "pipeline" in include:
            run_pipeline_matrix(p, rounds=rounds if not args.quick else 2, collect=collect_json)
    # Edge cases: empty, single row, ragged, late debut, entities, unicode, comments
    if "edge" in include:
        # Generate edge files on demand (tiny, not in main targets)
        edge_dir = BENCH_DATA / "edge"
        edge_dir.mkdir(exist_ok=True)
        run_edge_case_matrix(edge_dir, rounds=rounds, quick=args.quick, collect=collect_json)

    print("\nDone.")
    if args.output:
        out_path = Path(args.output)
        # If output is a directory, write to .benchmarks/crxml-<date>.json
        if out_path.is_dir() or args.output.endswith("/"):
            out_path = out_path / f"crxml-{_commit[:8]}.json"
        out_path.parent.mkdir(parents=True, exist_ok=True)
        # Add provenance
        payload = {
            "commit": _commit,
            "dirty": _dirty,
            "build_sha": getattr(_core, '__build_sha__', 'unknown'),
            "thp_defrag": _thp_value,
            "python": sys.version,
            "python_executable": sys.executable,
            "results": json_results,
        }
        out_path.write_text(_json.dumps(payload, indent=2), encoding='utf-8')
        print(f"Wrote {len(json_results)} records to {out_path} (commit {_commit} dirty={_dirty})")

def run_edge_case_matrix(edge_dir: Path, rounds=3, quick=False, collect=None):
    """Benchmark edge cases: empty, single row, ragged, sparse, truncated, entities, unicode, different row_tags.

    Edge cases always run in-process: files are <1 KB, subprocess overhead
    (0.3s) would dominate. Subprocess isolation is for files >=50 MB where
    cross-config contamination matters.
    """
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
    report(p_empty, "edge empty", lambda p=p_empty: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds, collect=collect)

    # 2) Single row
    single_xml = b'<?xml version="1.0"?><CrystalReport><Group><GroupHeader><Section/></GroupHeader><Group Level="2"><GroupHeader><Section SectionNumber="0"></Section></GroupHeader><Details Level="3"><Section SectionNumber="0"><Field Name="Field22" FieldName="{F}"><FormattedValue>1</FormattedValue><Value>1</Value></Field></Section></Details></Group></Group></CrystalReport>'
    p_single = write_edge("single_row", single_xml)
    report(p_single, "edge single row", lambda p=p_single: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds, collect=collect)

    # 3) Ragged: missing fields, late debut (FieldG appears only in last 10% rows)
    # Use existing 10MB file but test via drop_all vs sparse handling already in pushdown, here test ragged via bounded
    if not quick:
        p_10 = BENCH_DATA / "test_10mb.xml"
        if p_10.exists():
            report(p_10, "edge ragged via bounded64", lambda: _core.read_to_columnar_bounded(str(p_10), row_tag="Details", memory=64*1024), rounds=rounds, collect=collect)

    # 4) Entities & unicode
    ent_xml = b'<?xml version="1.0"?><CrystalReport><Group><Group Level="2"><GroupHeader/><Details Level="3"><Section SectionNumber="0"><Field Name="Field38" FieldName="{F}"><FormattedValue>A &amp; B &lt; C</FormattedValue><Value>A &amp; B &lt; C</Value></Field><Field Name="Field39"><FormattedValue>\xe2\x98\x83 unicode \xe2\x98\x85</FormattedValue><Value>\xe2\x98\x83 unicode \xe2\x98\x85</Value></Field></Section></Details></Group></Group></CrystalReport>'
    p_ent = write_edge("entities_unicode", ent_xml)
    report(p_ent, "edge entities+unicode", lambda p=p_ent: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds, collect=collect)

    # 5) Comment with fake row tag
    comment_xml = b'<?xml version="1.0"?><CrystalReport><!-- <Details Level="3"><Field Name="Trap"><Value>nope</Value></Field></Details> --><Group><Group Level="2"><GroupHeader/><Details Level="3"><Section SectionNumber="0"><Field Name="Field22"><Value>42</Value></Field></Section></Details></Group></Group></CrystalReport>'
    p_comment = write_edge("comment_fake_row", comment_xml)
    report(p_comment, "edge comment fake row", lambda p=p_comment: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds, collect=collect)

    # 6) Different row_tag: Row vs Details vs custom
    p_10 = BENCH_DATA / "test_10mb.xml"
    if p_10.exists():
        for tag in ["Details", "Row", "NonExistentTag"]:
            report(p_10, f"edge row_tag={tag}", lambda tag=tag, p=p_10: _core.read_to_columnar(str(p), row_tag=tag), rounds=rounds, collect=collect)

    # 7) Tiny file (1KB, few rows) vs large (already covered)
    tiny_xml = b'<?xml version="1.0"?><CrystalReport>' + b'<Details Level="3"><Section SectionNumber="0"><Field Name="F"><Value>1</Value></Field></Section></Details>'*5 + b'</CrystalReport>'
    p_tiny = write_edge("tiny_1kb", tiny_xml)
    report(p_tiny, "edge tiny 1KB", lambda p=p_tiny: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds, collect=collect)

    # 8) All engines on edge single row (stream vs columnar vs parallel vs bounded)
    p_single = edge_dir / "single_row.xml"
    for eng in (["stream","parallel"] if quick else ["stream","columnar","parallel"]):
        def fn(eng=eng, p=p_single):
            src = CrystalXMLSource(str(p), row_tag="Details", engine=eng)
            return src.to_arrow()
        report(p_single, f"edge single [{eng}]", fn, rounds=rounds, collect=collect)

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
            report(p_10, f"edge sink {sink}", fn, rounds=rounds, collect=collect)

    # 10) Streaming with 64KB vs 1MB on tiny file (constant memory)
    p_10 = BENCH_DATA / "test_10mb.xml"
    if p_10.exists():
        for mem in (["64KB","1MB"] if not quick else ["64KB"]):
            def fn(mem=mem, p=p_10):
                # Use new streaming iterator (true 64KB)
                it = _core.iter_record_batches(str(p), row_tag="Details", memory=mem)
                return sum(b.num_rows for b in it)
            report(p_10, f"edge stream {mem}", fn, rounds=rounds, collect=collect, mem=mem)

    # 11) Truncated / malformed (should not panic, return truncated row discarded)
    trunc_xml = b'<?xml version="1.0"?><CrystalReport><Group><Group Level="2"><Details Level="3"><Section SectionNumber="0"><Field Name="Field22"><Value>1</Value></Field></Section></Details></Group>'
    p_trunc = write_edge("truncated", trunc_xml)
    report(p_trunc, "edge truncated", lambda p=p_trunc: _core.read_to_columnar(str(p), row_tag="Details"), rounds=rounds, collect=collect)

    # 12) Field types bool/date32/timestamp via typed columns
    p_10 = BENCH_DATA / "test_10mb.xml"
    if p_10.exists() and not quick:
        for ftype in ["bool","date32"]:
            def fn(ftype=ftype, p=p_10):
                src = CrystalXMLSource(str(p), row_tag="Details", engine="parallel", field_types={"Field73": ftype})
                return src.to_arrow()
            report(p_10, f"edge typed {ftype}", fn, rounds=rounds, collect=collect, field_type=ftype)


if __name__ == "__main__":
    main()
