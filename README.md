<p align="center">
  <img src="https://raw.githubusercontent.com/emiliano-go/crxml/refs/heads/master/assets/icon.png" alt="crxml" width="225"/>
</p>
<p align="center">
  <em>Stream Crystal Reports XML at memory bandwidth.</em>
</p>
<p align="center">
  <h1 align="center">crxml</h1>
</p>

<p align="center">
  <strong>High-performance Crystal Reports XML → Arrow/DataFrame engine for Python.</strong>
</p>
<p align="center">
  Parse, filter, rename, cast, and project Crystal Reports XML directly into<br/>
  columnar data, with Rust execution, parallel parsing, bounded-memory<br/>
  processing, and automatic query fusion.
</p>

<p align="center">
  <a href="https://www.python.org/downloads/">
    <img src="https://img.shields.io/badge/Python-3.10%2B-3776AB?logo=python&logoColor=white&style=for-the-badge" alt="Python">
  </a>
  <a href="LICENSE">
    <img src="https://img.shields.io/badge/License-MIT-10AC84?style=for-the-badge" alt="License">
  </a>
  <a href="https://github.com/emiliano-go/crxml/actions/workflows/test.yml">
    <img src="https://img.shields.io/github/actions/workflow/status/emiliano-go/crxml/test.yml?branch=master&style=for-the-badge&logo=github&label=Tests" alt="Tests">
  </a>
  <a href="https://pypi.org/project/crxml/">
    <img src="https://img.shields.io/pypi/v/crxml?logo=pypi&logoColor=white&style=for-the-badge" alt="PyPI">
  </a>
  <a href="https://crxml.emiliano-go.com/">
    <img src="https://img.shields.io/badge/Docs-crxml.emiliano--go.com-8A2BE2?style=for-the-badge&logo=readthedocs" alt="Docs">
  </a>
</p>

---

## Quick start

```python
from crxml import CrystalXMLSource

source = CrystalXMLSource("report.xml", row_tag="Details")

# Row iteration: yields dicts lazily
for row in source:
    print(row["invoice"], row["amount"])

# DataFrame (auto-routes to parallel engine)
df = source.to_dataframe()
print(df.head())
```

That is it.  `df` is a pandas DataFrame with zero-copy ArrowDtype strings,
built in under a second for a 100 MB file.

With pipeline stages fused into the Rust parse loop:

```python
from crxml.stages import RenameFields, DropFields

pipeline = source | RenameFields({"f1": "invoice"}) | DropFields(["temp_id"])
df = pipeline.to_dataframe()
```

---

## Why crxml

> This library was originally inspired by [carlosplanchon/xmlstreamer](https://github.com/carlosplanchon/xmlstreamer).

Crystal Reports XML exports are deeply nested: `<Group>` wraps `<GroupHeader>`
wraps `<Section>` wraps `<Details>` wraps `<Field>`/`<Text>`/`<FormattedValue>`/
`<Value>`/`<TextValue>`.  Standard XML libraries (ElementTree, SAX, lxml)
spend most of their CPU time descending into children you do not need.

crxml skips the nesting:
- The **stream engine** walks the XML once with a hand-rolled `memchr` scanner (`src/crxml_core/src/xml/scanner.rs`, `scan_one_row` `scanner.rs:81` via `RowSink` `src/crxml_core/src/lib.rs:564`) and yields flat dicts: **508 MB/s** 100 MB (was 251 `quick-xml`).
- The **parallel engine** memory-maps the file, splits it at row boundaries (`splitter.rs:27` `find_split_points`), and parses each chunk on its own thread into Arrow buffers directly (no dicts): **up to 4.2 GB/s** on high-cardinality production reports (533 MB real `par128` 4198) and **4.1 GB/s** on uniform exports (1 GB `par96` 4099) via `rypipe` (`rypipe-core` `Vec<ColumnBuilder>`+`field_index` `engine.rs:16`, `row_dirty` `engine.rs:26`).
  It is powered by the [rypipe](https://github.com/emiliano-go/rypipe) ingestion engine: `rypipe` itself was **extracted from `crxml`**: the original `crxml` engine was the prototype, then separated and abstracted so any format (CSV, JSON, HTML…) could reuse it. `crxml` now lives as a thin adapter (`crxml-core`) on top of `rypipe-core`.
- Pipeline stages that rename, cast, drop, or filter fields execute in the Rust
  parse loop, before any Python object is created.

---

## Comparison: stream vs parallel vs parallel streaming (bounded, frozen schema)

| Task | stream (single) | parallel (full RAM) | **parallel streaming (bounded)** |
|---|---|---|---|
| Row iteration | Yields dicts lazily | Arrow table first, then dicts (slower) | Yields `RecordBatch`es incrementally, stable schema |
| DataFrame / Table output | Collects dicts, converts | Direct Arrow buffers, zero-copy | Same, incremental + bounded |
| 533 MB real export (Table) | 745 MB/s (single) / 723 MB/s (1 MB) | **4470 MB/s** `par128` (4.16 MB) | 3828 auto / **4980 explicit `schema=[...]`** (2 MB) |
| 1 GB synthetic (Table) | 734 MB/s | 4278 MB/s `par128` | 3782 auto / ~4900 explicit |
| Peak RssAnon (533 MB) | 24 MB (1 MB) | 137 MB | **88 MB** (auto or explicit) |
| Pipeline fusion | No (dict path) | Yes (Rust BuildPlan) | Yes (same plan, streamed) |
| `ParquetWriter` | N/A | N/A | `write_batch` now succeeds (batches share frozen schema `schema.rs:14`); before fix batch 2 order `FieldG` vs `Text20` last raised |

Auto discovery (16×2 MiB windows for >128 MB) adds ~15% (≈19 ms on 533 MB) so auto is **−14% vs par128** (3828 vs 4470) but still bounded and incremental. Explicit `schema=[...]` (`FrozenSchema::from_plan`) avoids Discovery and is **+11% vs par128** (4980 vs 4470). The old "fast or memory-safe" is now "fastest bounded needs explicit schema; auto is safe and bounded but slightly slower". Use `iter_record_batches(memory="64MB", threads=16, schema=[...])` for the fast path.

[Full benchmark details](docs/performance.md): like-for-like Table vs Vec, chunk-per-cell, fixed-chunk isolation, and frozen-schema cost.

---

## Install

```bash
pip install crxml
```

The columnar and parallel engines are included by default.  For performance
profiling counters: `pip install -e . --config-settings=--features=profile`.

---

## Features

| Category | What crxml handles |
|---|---|
| **Stream engine** | Row-by-row XML parsing, yields `dict[str, str]`, GIL-released batching |
| **Columnar engine** | Single-threaded Arrow table output, zero-copy string columns |
| **Parallel engine** | Multi-threaded (rayon), file split at row boundaries, off-GIL parse |
| **Bounded mode** | `memory="500MB"` splits into chunks; RSS independent of file size |
| **Pipeline fusion** | `RenameFields`, `DropFields`, `CastTypes`, `FilterRows` compile into Rust BuildPlan |
| **mmap** | Memory-maps input files (default, zero-copy) |
| **prefault** | `MADV_WILLNEED` vs `MADV_SEQUENTIAL` for RSS/speed trade-off |
| **Arrow sinks** | `to_arrow()`, `to_pandas()` (ArrowDtype), `to_polars()`, `to_parquet()` |
| **Auto-dict encoding** | `auto_dict=True` encodes low-cardinality string columns |
| **Field typing** | `field_types={"amount": "float64"}` coerces at parse time |
| **Filter pushdown** | `filter={"field": "Status", "op": "==", "value": "Active"}` in Rust |
| **Correctness** | All engines validated byte-identical against stream oracle (29 test cases + 465k-row real cross-check) |

---

## Engine guide: parallel streaming (frozen schema) is opt-in

| Engine / API | When to use | Throughput 533 MB / 1 GB | RssAnon |
|---|---|---|---|
| `stream` (`for row in source`) | Row-by-row dict iteration | 723 MB/s 1 MB budget (24 MB anon) | 24 MB |
| `columnar` (`single`) | Single-threaded Arrow Table | 745 / 734 MB/s | 134 MB |
| `parallel` (`par128` full RAM, 4 MB) | Fastest full-RAM Table | **4470 / 4278 MB/s** | 137 MB |
| **`iter_record_batches(..., threads=16, schema=[...])` (explicit frozen)** | **Fastest bounded, stable schema**, yields `RecordBatch`es | **4980 / ~4900 MB/s** | **88 MB** |
| `iter_record_batches(memory="64MB", threads=16)` auto | Bounded + incremental, stable schema | 3828 / 3782 MB/s (−14% vs par, +15% Discovery) | **88 MB** |
| `bounded` (`memory="64MB"` single) | Single-thread bounded | 645 / 546 MB/s | 133 MB |

Pass `engine=` explicitly, or let `auto` select per call. `auto` stays **"parallel if it fits"** (blocked: auto discovery adds 15% and would make `auto` slower until cheaper). Streaming is **opt-in** via `iter_record_batches(..., threads=16)`, keeping 4 MB for `par` (`src/crxml/source.py:155`), 2 MB via `budget/(threads×2)` for streaming. Provide `schema=` for the fast path.

```python
# Recommended bounded paths
from crxml import CrystalXMLSource
import pyarrow as pa, pyarrow.parquet as pq

src = CrystalXMLSource("report.xml", row_tag="Details")
# explicit schema: fastest, no Discovery, writer succeeds
schema = ["Level","Section","Field22","Field23","Field38","Field39","Field61","Field73","FieldG","Text20"]
batches = src.iter_record_batches(memory="64MB", threads=16, schema=schema) # not yet wired in Python, use _core
# auto: stable but pays 15% Discovery (16×2 MiB windows for >128 MB)
batches = src.iter_record_batches(memory="64MB", threads=16)

# ParquetWriter (now works; batches share frozen schema)
it = src.iter_record_batches(memory="64MB", threads=16)
first = next(it)
w = pq.ParquetWriter("out.parquet", first.schema)
w.write_batch(first)
for b in it: w.write_batch(b)
w.close()
```

---

## Framework support

| Framework | Integration |
|---|---|
| **FastAPI** / **Starlette** / **Litestar** | Parse in route handler, return DataFrame or Arrow table directly |
| **Django** / **Flask** | Call `source.to_dataframe()` in view; pass to template or response |
| **Pandas / Polars** | `source.to_dataframe()` / `source.to_polars()` for zero-copy analysis |
| **Airflow / Prefect** | Parse in task, write to parquet with `source.to_parquet()` |
| **CLI / ETL scripts** | Use `to_csv()` sink or iterate rows for line-by-line processing |

---

## Limitations

- **UTF-8 input only.** UTF-16 exports (which Crystal Reports can produce) fail validation; convert first.
- **No compressed input.** `.gz`/`.zst` files must be decompressed before parsing.
- **Crystal Reports grammar, not general XML.** The flat-row model fits CR exports; arbitrary XML documents are out of scope.
- **Linux-tuned performance.** madvise hints and thread-count ratios were measured on Linux; other platforms work but are untested territory.
- **No async API.** Row iteration is synchronous.

---

## Documentation

Full docs at [crxml.emiliano-go.com](https://crxml.emiliano-go.com/) covering:

- All `CrystalXMLSource` parameters
- Pipeline stages and fusion rules
- Sink reference
- Batch iteration and parallel distribution
- [Performance](docs/performance.md) with phase breakdowns
- Architecture and correctness

## License

MIT
