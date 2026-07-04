# Performance

Benchmarked on a 100 MB synthetic Crystal Reports XML file (90,384 rows,
~10 fields/row) using a release build (`opt-level=3, lto=true`).

**Test machine:** 13th Gen Intel i5-1335U (12 cores), 15 GB RAM, Linux.

## Native export (Rust-only, no Python dicts)

These call the columnar engine directly. This is the fastest path when the goal is
an Arrow table or DataFrame.

| Function | Time | Rows/s | MB/s |
|----------|------|--------|------|
| `read_to_columnar` (single-threaded) | 2.14s | 42 K | 47 |
| `read_to_columnar_multi` (2 chunks) | 1.79s | 50 K | 56 |
| `read_to_columnar_par` (12 threads) | **0.80s** | **113 K** | **125** |

## Source row iteration

`for row in source` yields `dict[str, str]`. The stream path uses a batched
Rust reader with GIL release (Phase 1). Columnar/parallel iteration
reconstructs dicts from Arrow tables and is slower. Those engines are
designed for table output, not row iteration.

| Engine | Time | Rows/s | MB/s |
|--------|------|--------|------|
| Stream | **1.84s** | **49 K** | 54 |
| Columnar | 11.65s | 7.8 K | 8.6 |
| Parallel | 10.77s | 8.4 K | 9.3 |

## Arrow export

`source.to_arrow()` returns a `pyarrow.Table`.

| Engine | Time | Rows/s | MB/s |
|--------|------|--------|------|
| Stream | 1.89s | 48 K | 53 |
| Columnar | 1.42s | 63 K | 70 |
| Parallel | **0.62s** | **147 K** | **162** |

## DataFrame export

`source.to_dataframe()` → `pd.DataFrame` with `pd.ArrowDtype` by default.
`collect() → list[dict]`.

| Engine | Time | Rows/s | MB/s |
|--------|------|--------|------|
| Stream | 2.27s | 40 K | 44 |
| Columnar | 1.29s | 70 K | 77 |
| Parallel | **0.69s** | **131 K** | **144** |

## Auto routing (recommended)

With `engine="auto"` (the default) the engine selection is **goal-aware**:

- **Row iteration** → always uses the stream (dict) path
- **Arrow / DataFrame / collect** → routes to parallel (when available and
  file ≥ 8 MB) or single-threaded columnar, bypassing the dict loop entirely.

This means `source.to_dataframe()` with auto routing runs at **~0.69s**
(131 K rows/s), over **3× faster** than the old stream-based dict path.
DataFrames use `pd.ArrowDtype` for zero-copy string columns by default.

Row iteration uses a configurable `batch_size` (default 1024) to control
Python↔Rust boundary crossings. Set via `CrystalXMLSource(..., batch_size=N)`.

## Where time goes (profiled via Instant counters, stream engine)

Profile counters are built in with the `profile` feature
(`maturin build --features profile`). The three instrumented hot paths
account for ~67% of wall time on a 100 MB file (90,384 rows).

| Layer | ns/row | % of instrumented | % of wall |
|-------|--------|-------------------|-----------|
| XML event loop (`quick-xml`) | 12,702 | 69% | 46% |
| Unescape (attribute + text) | 3,549 | 19% | 13% |
| Dict build (PyDict + set_item) | 2,077 | 12% | 8% |
| **Sum instrumented** | **18,328** | **100%** | **67%** |
| Uninstrumented (Python iter, call overhead, buf alloc) | ~9,149 | n/a | 33% |

The ~33% unmeasured gap comes from Python iteration protocol, `__next__` /
`next_row` dispatch, GIL acquire/release, and buffer allocation/copying.
No single remaining hot path exceeds ~5% of wall time.

The columnar/parallel engine bypasses both the dict build **and** the
unescape step by writing directly into Arrow buffers. That is where the
3x speedup comes from.

## Pipeline fusion (Phase 4)

Fusable stages (`RenameFields`, `DropFields`, `CastTypes` with standard
types) are compiled into the columnar `BuildPlan` and run in Rust during
parsing. Non-fusable stages (lambdas, custom predicates) apply to dicts
after Arrow conversion.

| Pipeline (4 stages) | Time vs dict-only | Speedup |
|---------------------|-------------------|---------|
| All fusable | ~same as bare columnar | ~3× vs dict pipeline |
| Mixed (fusable + lambda) | columnar parse + lambda on dicts | ~2× vs dict pipeline |

## Memory

| Scenario | RSS | Py objects |
|----------|-----|------------|
| Row iteration (stream) | ~430 MB | 2 MB |
| Native export | ~420 MB | 0 MB |
| DataFrame (parallel) | ~434 MB | 3 MB |

RSS is dominated by the 100 MB XML file buffered in page cache and the
columnar engine's intermediate builders. Python heap usage is minimal.

## Recommendations

| Goal | Engine | Notes |
|------|--------|-------|
| Row iteration (`for row in source`) | `"auto"` → stream | |
| Arrow / DataFrame | `"auto"` → parallel (≥8 MB) or columnar | Fastest path |
| Pipeline with fusable stages | `"auto"` → columnar/parallel | Stages fused into BuildPlan |
| Pipeline with lambdas | `"auto"` → columnar + dict tail | Lambda overhead only |
