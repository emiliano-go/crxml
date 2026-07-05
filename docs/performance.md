# Performance

Benchmarked on a 100 MB synthetic Crystal Reports XML file (90,384 rows,
~10 fields/row) using a release build (`opt-level=3, lto=true`).

All throughput numbers are averaged over 5 runs in fresh subprocesses (one
Python process per measurement) to isolate page cache effects. Row count
(90,384) is asserted inside each timed block so no measurement can silently
return from cache.

**Test machine:** 13th Gen Intel i5-1335U (12 cores), 15 GB RAM, Linux.

## Native export (Rust-only, no Python dicts)

These call the columnar engine directly via Rust FFI and produce a
`pyarrow.Table`. This is the fastest path when the goal is an Arrow table
or DataFrame — no Python dicts are ever created.

| Function | Time | Rows/s | MB/s |
|----------|------|--------|------|
| `read_to_columnar` (single-threaded) | 0.42s | 217 K | 252 |
| `read_to_columnar_multi` (2 chunks) | 0.18s | 504 K | 585 |
| `read_to_columnar_par` (12 threads) | **0.07s** | **1.26 M** | **1.46 G** |

## Source row iteration

`for row in source` yields `dict[str, str]`. The stream path uses a batched
Rust reader with GIL release. Columnar/parallel iteration reconstructs dicts
from Arrow tables and is slower — those engines exist for table output.

| Engine | Time | Rows/s | MB/s |
|--------|------|--------|------|
| Stream | **0.50s** | **181 K** | 209 |
| Columnar | 0.62s | 145 K | 160 |
| Parallel | 0.46s | 196 K | 216 |

## Arrow export

`source.to_arrow()` returns a `pyarrow.Table`.

| Engine | Time | Rows/s | MB/s |
|--------|------|--------|------|
| Stream | 0.50s | 180 K | 209 |
| Columnar | 0.16s | 558 K | 647 |
| Parallel | **0.05s** | **1.80 M** | **2.09 G** |

## DataFrame export

`source.to_dataframe()` returns a `pd.DataFrame` with `pd.ArrowDtype`
(zero-copy string columns).

| Engine | Time | Rows/s | MB/s |
|--------|------|--------|------|
| Stream | 0.55s | 163 K | 189 |
| Columnar | 0.15s | 590 K | 684 |
| Parallel | **0.04s** | **2.29 M** | **2.66 G** |

## Auto routing (recommended)

With `engine="auto"` (the default) the engine selection is **goal-aware**:

- **Row iteration** → always uses the stream (dict) path
- **Arrow / DataFrame / collect** → routes to parallel (when available and
  file >= 8 MB) or single-threaded columnar, bypassing the dict loop
  entirely.

This means `source.to_dataframe()` with auto routing runs at **~0.04s**
(2.3 M rows/s) for files >= 8 MB, over **50x faster** than the original
stream-based dict path. DataFrames use `pd.ArrowDtype` for zero-copy string
columns by default.

## Pipeline fusion

Fusable stages (`RenameFields`, `DropFields`, `CastTypes` with standard
types) are compiled into the columnar `BuildPlan` and run in Rust during
parsing. Non-fusable stages (lambdas, custom predicates) apply to dicts
after Arrow conversion, or run through the vectorized batch chain.

| Pipeline (4 stages) | Time vs dict-only | Speedup |
|---------------------|-------------------|---------|
| All fusable | ~same as bare columnar | ~3x vs dict pipeline |
| Mixed (fusable + lambda) | columnar parse + lambda on dicts | ~2x vs dict pipeline |

## Memory

| Scenario | RSS | Py objects |
|----------|-----|------------|
| Row iteration (stream) | ~430 MB | < 2 MB |
| Native export (parallel) | ~420 MB | 0 MB |
| DataFrame (parallel) | ~434 MB | < 3 MB |

RSS is dominated by the 105 MB XML file buffered in page cache and the
columnar engine's intermediate builders. Python heap usage is minimal.

## Recommendations

| Goal | Engine | Notes |
|------|--------|-------|
| Row iteration (`for row in source`) | `"auto"` -> stream | |
| Arrow / DataFrame | `"auto"` -> parallel (>= 8 MB) or columnar | Fastest path |
| Pipeline with fusable stages | `"auto"` -> columnar/parallel | Stages fused into BuildPlan |
| Pipeline with lambdas | `"auto"` -> columnar + dict tail | Lambda overhead only |
