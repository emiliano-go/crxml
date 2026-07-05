<p align="center">
  <a href="https://www.python.org/downloads/">
    <img src="https://img.shields.io/badge/Python-3.10%2B-3776AB?logo=python&logoColor=white&style=for-the-badge" alt="Python">
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/License-MIT-10AC84?style=for-the-badge" alt="License">
  </a>
  <a href="https://pypi.org/project/crxml/">
    <img src="https://img.shields.io/pypi/v/crxml?logo=pypi&logoColor=white&style=for-the-badge" alt="PyPI">
  </a>
</p>

# crxml

Fast streaming parser for Crystal Reports XML exports.

```python
from crxml import CrystalXMLSource, to_dataframe

df = to_dataframe(CrystalXMLSource("report.xml", row_tag="Details"))
print(df.head())
```

## Installation

**Prerequisites:** Python 3.10 or later, and [Rust](https://rustup.rs).

```bash
pip install crxml
```

To enable the columnar/parallel engines and pipeline fusion:

```bash
pip install -e . --config-settings=--features=columnar
```

To enable Instant-based performance counters for profiling:

```bash
pip install -e . --config-settings=--features=profile  # see bench_profile.py
```

## About

crxml streams through Crystal Reports XML files row by row, never loading the full document into memory. It extracts field data from nested CR field elements and yields flat dictionaries. A built-in pipeline lets you rename, cast, filter, and drop fields with pipe operators.

The Rust backend parses 100 MB in about 0.8 seconds (parallel columnar) to Arrow tables. Row iteration runs at about 1.8 seconds using the stream engine (dict path).

Fusable pipeline stages (rename, cast, drop, filter by predicate) are compiled into the columnar BuildPlan and run in Rust during parsing, avoiding the dict round-trip. Non-fusable stages (lambdas, custom predicates) apply after Arrow conversion.

This library is conceptually based on [carlosplanchon/xmlstreamer](https://github.com/carlosplanchon/xmlstreamer).

## Quick start

```python
from crxml import CrystalXMLSource, to_dataframe

source = CrystalXMLSource("report.xml", row_tag="Details")

# Row iteration
for row in source:
    print(row)

# DataFrame (routes to parallel engine automatically)
df = to_dataframe(source)
```

## CrystalXMLSource

The source object is the entry point for all parsing. It accepts these parameters:

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `source` | str or Path | required | Path to the Crystal Reports XML file. |
| `row_tag` | str | `"Row"` | XML element tag that delimits each record. For Crystal Reports exports this is often `"Row"` or `"Details"`. |
| `engine` | str | `"auto"` | Engine selection: `"auto"`, `"stream"`, `"columnar"`, or `"parallel"`. |
| `threads` | int | 0 | Number of chunks for parallel parsing (0 = CPU count). |
| `memory` | int or str | None | Memory budget for bounded parsing. Accepts bytes (int) or strings like `"8GB"`. |
| `field_mapping` | dict | None | Rename fields at parse time: `{"old_name": "new_name"}`. |
| `drop_fields` | list | None | Fields to omit from output. |
| `filter` | dict | None | Rust-side filter: `{"field": "Status", "op": "==", "value": "Active"}`. |
| `field_types` | dict | None | Coerce fields at parse time: `{"Score": "int64", "Amount": "float64"}`. |
| `dictionary_columns` | list | None | Fields to dictionary-encode during columnar parse. |
| `schema` | list | None | Ordered list of fields to include (others dropped). |
| `auto_dict` | bool | False | Automatically dictionary-encode string columns. |
| `use_mmap` | bool | False | Memory-map the file instead of reading into a buffer. |
| `batch_size` | int | 1024 | Rows fetched per Rust call during batched iteration. |

```python
from crxml import CrystalXMLSource

source = CrystalXMLSource(
    "report.xml",
    row_tag="Details",
    batch_size=2048,
    field_mapping={"f1": "invoice", "f2": "amount"},
    drop_fields=["tax_rate", "internal_id"],
    field_types={"amount": "float64", "quantity": "int64"},
    dictionary_columns=["product_code"],
    memory="4GB",
)
```

### File/string-like objects

The `source` parameter accepts a file path (string or `pathlib.Path`). File-like objects with a `.name` attribute pointing to an existing file are also supported.

## Engine selection

The parser has three backend engines, plus an auto mode that selects the best engine per goal.

| Engine | Requires | Description |
|--------|----------|-------------|
| `stream` | None | Row-by-row XML parsing. Produces dicts directly. Always available. |
| `columnar` | `columnar` feature | Single-threaded Arrow columnar parse. Zero-copy string columns. |
| `parallel` | `columnar` feature | Multi-threaded columnar parse (file is split into chunks). |

### Auto routing

When `engine="auto"` (the default), the engine is resolved per call based on the goal and file size:

**For row iteration** (`for row in source`): always uses the stream engine. Batched parsing with GIL release (phase 1) handles the iteration efficiently.

**For table output** (`source.to_arrow()`, `source.to_dataframe()`): resolves in this priority order:
1. Parallel engine (file >= 8 MB, columnar feature available, within memory budget)
2. Columnar engine (columnar feature available, within memory budget)
3. Stream fallback (small files, or columnar not available)

```python
source = CrystalXMLSource("large_report.xml", engine="auto")

for row in source:         # always stream engine (dicts)
    pass

df = source.to_dataframe() # auto resolves to parallel (large file)
```

When the columnar or parallel engine is used for table output, the dict construction overhead is eliminated entirely. Strings are written directly into Arrow buffers. This gives a 3x speedup over the stream-based dict path.

### Explicit engine

You can also pick an engine explicitly:

```python
source = CrystalXMLSource("report.xml", engine="parallel", threads=8)
table = source.to_arrow()   # uses parallel engine
```

## Row iteration

`CrystalXMLSource` is iterable. It yields dictionaries mapping field names to string values.

```python
source = CrystalXMLSource("report.xml")

for row in source:
    print(row["invoice"], row["amount"])
```

### How it works

For the stream engine (the iter goal always uses stream), iteration goes through a `_BatchIter` wrapper. Each call to `next()` fetches a batch of rows (configurable via `batch_size`) from Rust with the GIL released, then returns rows one by one from an internal buffer. This reduces Python/Rust boundary crossings from one-per-row to one-per-batch.

When the columnar or parallel engine is explicit and you iterate, the full Arrow table is parsed first and dicts are reconstructed from it. That path is slower and exists only for compatibility. Table-oriented callers should use `to_arrow()` or `to_dataframe()` directly.

### Batched iteration

The `_iter_batches` method yields lists of dicts instead of single rows:

```python
for batch in source._iter_batches(batch_size=4096):
    for row in batch:
        print(row)
```

This is used internally by pipeline fusion and the `collect` sink.

## Table output

These methods produce Arrow tables or DataFrames. All of them use the resolved table engine (columnar/parallel when available).

### to_arrow

Returns a `pyarrow.Table` of the parsed data. Zero-copy for columnar/parallel engines. For the stream engine, dicts are collected and converted to a table.

```python
table = source.to_arrow()
print(table.num_rows, table.column_names)
```

### to_dataframe / to_pandas

`to_dataframe()` is an alias for `to_pandas()`. Both return a pandas DataFrame.

```python
df = source.to_dataframe()            # ArrowDtype columns (zero-copy)
df = source.to_pandas(arrow_backed=False)  # numpy-backed strings
```

By default (`arrow_backed=True`), string columns use `pd.ArrowDtype` for zero-copy conversion from Arrow buffers. This requires pandas 1.5 or later. Pass `arrow_backed=False` to materialize strings as Python str objects.

### to_polars

```python
import polars as pl
df = source.to_polars()  # zero-copy from Arrow
```

### to_parquet

```python
source.to_parquet("output.parquet")
# Forward kwargs to pyarrow.parquet.write_table
source.to_parquet("out.parquet", compression="zstd")
```

### schema

Returns the field names from the first row:

```python
fields = source.schema()  # ["invoice", "amount", ...]
```

## Pipeline stages

Stages transform the row stream. They are chained with the pipe operator `|`.

```python
from crxml.stages import RenameFields, CastTypes, DropFields, FilterRows

pipeline = (
    CrystalXMLSource("report.xml")
    | RenameFields({"f1": "invoice", "f2": "amount"})
    | CastTypes({"amount": float})
    | DropFields("tax_rate")
    | FilterRows(lambda r: r["amount"] > 100)
)

for row in pipeline:
    print(row)
```

### RenameFields

Renames dictionary keys.

```python
RenameFields({"old_name": "new_name", ...})
```

Fusable: yes. Compiled into the columnar BuildPlan field_mapping.

### CastTypes

Casts field values to target types.

```python
CastTypes({"amount": float, "quantity": int})
```

Fusable when the target type is one of: `int`, `float`, `bool`, `str`. Non-standard callables (lambdas, custom functions) force the stage to stay on the dict path.

```python
CastTypes({"amount": float})            # fusable
CastTypes({"amount": lambda v: v * 2})  # not fusable, stays on dict path
```

### DropFields

Removes fields from each row.

```python
DropFields(["tax_rate", "internal_id"])
```

Fusable: yes. Compiled into the BuildPlan drop_fields.

### FilterRows

Filters rows by a predicate. Supports three forms:

```python
# Lambda predicate (not fusable)
FilterRows(lambda r: r["amount"] > 100)

# Constant field comparison (fusable)
FilterRows(field="Status", op="==", value="Active")
FilterRows(field="Quantity", op="!=", value="0")

# Column-to-column comparison (fusable)
FilterRows(field_a="Price", op=">", field_b="Cost")
FilterRows(field_a="Name", op="eq", field_b="ExpectedName")
```

Fusable when using the keyword-argument form (constant comparison or column-to-column comparison). Lambda predicates always apply after Arrow conversion.

### Custom stages

Any callable that accepts and returns an iterable of dicts is a valid stage:

```python
def add_metadata(stream):
    for row in stream:
        row["source"] = "crystal_reports"
        yield row

pipeline = CrystalXMLSource("report.xml") | add_metadata
```

Custom stages are never fused. They always run on dicts after Arrow conversion.

## Pipeline fusion

When a pipeline contains fusible stages, the library tries to push them into the columnar BuildPlan so they execute in Rust during parsing. This avoids the cost of constructing dicts from the Arrow table and then immediately renaming/casting/dropping fields.

### How fusion works

1. `Pipeline.__iter__` calls `fused_iter(source, stages)`.
2. `_try_columnar_fusion` inspects each stage for a `_plan_kwargs` method.
3. Stages that return kwargs are merged into the columnar BuildPlan.
4. The plan is passed to `source._read_arrow(plan_overrides=...)`.
5. Non-fusable stages apply to the resulting dict stream.

### What fuses

| Stage | Fusable kwargs | Condition |
|-------|---------------|-----------|
| `RenameFields` | `field_mapping` | Always |
| `CastTypes` | `field_types` | Only for int, float, bool, str targets |
| `DropFields` | `drop_fields` | Always |
| `FilterRows` | `filter` | Only for keyword-argument form |

### What does not fuse

- Lambda predicates in `FilterRows`
- Custom stage functions
- `CastTypes` with non-standard callables
- Stages on an iterable that is not a `CrystalXMLSource` (no `_read_arrow` method)

### Dict-path fallback

When no stage can be fused, the library falls back to the dict path. Fusible stages are applied inline (one function call per row) using batched iteration (`_iter_batches`). Non-fusable stages wrap the stream as callables. This path still benefits from batched Rust parsing with GIL release.

```python
# All fusable: runs entirely in Rust (columnar plan)
pipeline = source | RenameFields({"a": "b"}) | DropFields(["x"])

# Mixed: columnar parse + lambda on dicts
pipeline = source | RenameFields({"a": "b"}) | FilterRows(lambda r: r["b"] > 0)

# No fusible: dict path with batched iteration
pipeline = source | FilterRows(lambda r: r["amount"] > 0)
```

### The _arrow_iter compatibility helper

When columnar or parallel engines produce row iterators, the `_arrow_iter` function reconstructs dicts from the Arrow table. This is a compatibility path for callers that request row iteration on a columnar-able source. Table-oriented callers (to_arrow, to_dataframe) bypass this entirely.

## Sinks

Sinks consume an iterable of dicts and produce a concrete result.

```python
from crxml import to_dataframe, to_csv, collect
```

### to_dataframe

Converts an iterable of dicts (source or pipeline) to a pandas DataFrame.

```python
df = to_dataframe(source)
df = to_dataframe(pipeline)
```

DataFrames use `pd.ArrowDtype` for zero-copy string columns (pandas 1.5+).

### to_csv

Writes rows to a CSV file.

```python
to_csv(pipeline, "output.csv", delimiter=",")
```

### collect

Collects all rows into a list.

```python
rows = collect(pipeline)
```

Uses batched iteration (`_iter_batches`) when available for efficiency.

## Parallel mode

Pipelines can be distributed across worker processes.

```python
pipeline = (
    CrystalXMLSource("report.xml")
    | RenameFields({"f1": "invoice"})
)

pipeline = pipeline.parallel(workers=4, batch_size=1000)
for row in pipeline:
    print(row)
```

This splits the file into chunks and processes each chunk in a subprocess. Stages must be picklable (standard stages are). Lambda predicates are not picklable and will raise an error.

## Feature flags

Some capabilities require Cargo feature flags at build time.

| Feature | Build command | What it enables |
|---------|--------------|-----------------|
| `columnar` | `pip install -e . --config-settings=--features=columnar` | Columnar engine, parallel engine, pipeline fusion into BuildPlan, zero-copy Arrow output. |
| `profile` | `pip install -e . --config-settings=--features=profile` | Instant-based performance counters in the stream engine. Use `CrxmlReader.get_profile_data()` or `bench_profile.py` to read them. |

## Architecture overview

### Two parse paths

crxml has two fundamentally different parse paths:

**Stream path** (always available): The Rust `CrxmlReader` walks the XML with `quick-xml`, extracting field names and values. Batching (via `next_batch`) parses groups of rows with the GIL released, then builds Python dicts in one shot. This is the path used for row iteration.

**Columnar path** (requires `columnar` feature): The Rust code writes parsed field data directly into Arrow buffers. There are no intermediate Python dicts. The columnar engine also accepts a `BuildPlan` with field mapping, field types, drop list, dictionary columns, and filter predicates, allowing fusible pipeline stages to execute entirely in Rust.

### Goal-aware routing

The `CrystalXMLSource` routes calls based on their goal:

- `__iter__` and `_iter_batches` use the resolution for goal `"iter"` (always stream in auto mode).
- `to_arrow`, `to_dataframe`, `to_pandas`, `to_polars`, and `to_parquet` use the resolution for goal `"table"` (columnar or parallel when available).

This means iterating rows and building a DataFrame from the same source use different internal engines, each optimal for its goal.

Python layer manages the routing, caching, and dict conversion. The Rust layer handles XML parsing, field extraction, Arrow buffer construction, and parallel chunking.

## Benchmarks

Measured on a 100 MB synthetic Crystal Reports XML file (90,384 rows, about 10 fields per row) with a release build (opt-level=3, lto=true).

**Test machine:** 13th Gen Intel i5-1335U (12 cores), 15 GB RAM, Linux (hostname: osiris).

### Native export (Rust only, columnar engine)

| Function | Time | Rows/s | MB/s |
|----------|------|--------|------|
| `read_to_columnar` | 2.14s | 42 K | 47 |
| `read_to_columnar_par` (12 threads) | **0.80s** | **113 K** | 125 |

### Source iteration

`for row in source` yields `dict[str, str]`.

| Engine | Time | Rows/s | MB/s |
|--------|------|--------|------|
| Stream | 1.84s | 49 K | 54 |
| Columnar | 10.77s | 8.4 K | 9.3 |

Columnar/parallel iteration reconstructs dicts from Arrow tables and is slower. Those engines are designed for table output, not row iteration.

### Arrow / DataFrame

| Engine | Arrow Time | DataFrame Time |
|--------|------------|----------------|
| Stream | 1.89s | 2.27s |
| Columnar | 1.42s | 1.29s |
| Parallel | **0.62s** | **0.69s** |

With `engine="auto"` (default), `source.to_dataframe()` routes to parallel automatically, giving **0.69s / 131 K rows/s** for a 100 MB file. This is over 3x faster than the stream-based dict path.

### Where time goes (profiled via Instant counters, stream engine)

Profile counters are built in with the `profile` feature. The three instrumented hot paths account for about 67% of wall time on a 100 MB file (90,384 rows).

| Layer | ns/row | % of instrumented | % of wall |
|-------|--------|-------------------|-----------|
| XML event loop (quick-xml) | 12,702 | 69% | 46% |
| Unescape (attribute + text) | 3,549 | 19% | 13% |
| Dict build (PyDict + set_item) | 2,077 | 12% | 8% |
| **Sum instrumented** | **18,328** | **100%** | **67%** |
| Uninstrumented | about 9,149 |  | 33% |

The unmeasured gap comes from Python iteration protocol, `__next__` and `next_row` dispatch, GIL acquire/release, and buffer allocation. No single remaining hot path exceeds 5% of wall time.

The columnar/parallel engine bypasses both the dict build and the unescape step by writing directly into Arrow buffers. That is where the 3x speedup comes from.

### Pipeline fusion

Fusable stages (RenameFields, DropFields, CastTypes with standard types) are compiled into the columnar BuildPlan and run in Rust during parsing. Non-fusable stages (lambdas, custom predicates) apply to dicts after Arrow conversion.

| Pipeline (4 stages) | Time vs dict-only | Speedup |
|---------------------|-------------------|---------|
| All fusable | Same as bare columnar | About 3x vs dict pipeline |
| Mixed (fusable + lambda) | Columnar parse + lambda on dicts | About 2x vs dict pipeline |

### Memory

| Scenario | RSS | Py objects |
|----------|-----|------------|
| Row iteration (stream) | about 430 MB | 2 MB |
| Native export | about 420 MB | 0 MB |
| DataFrame (parallel) | about 434 MB | 3 MB |

RSS is dominated by the 100 MB XML file buffered in page cache and the columnar intermediate builders. Python heap usage is minimal.

### Recommendations

| Goal | Engine | Notes |
|------|--------|-------|
| Row iteration (`for row in source`) | `"auto"` (stream) | Best for row-by-row processing |
| Arrow / DataFrame | `"auto"` (parallel or columnar) | Fastest path, 3x over stream |
| Pipeline with fusable stages | `"auto"` (columnar/parallel) | Stages fused into BuildPlan |
| Pipeline with lambdas | `"auto"` (columnar + dict tail) | Only lambda overhead on dicts |

## Publishing

```bash
./upload.sh
```

Builds a manylinux2014 wheel and sdist and uploads to PyPI. Requires `maturin` and `twine`. The `--manylinux 2014 --zig` flag ensures PyPI-compatible platform tags.

## Documentation

Full documentation is available at the [project site](https://crxml.emiliano-go.com/), covering installation, usage, stages, custom stages, architecture, performance, FastAPI integration, and the Rust core.

## License

MIT
