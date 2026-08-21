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

Crystal Reports XML exports are deeply nested: `<Group>` wraps `<GroupHeader>`
wraps `<Section>` wraps `<Details>` wraps `<Field>`/`<Text>`/`<FormattedValue>`/
`<Value>`/`<TextValue>`.  Standard XML libraries (ElementTree, SAX, lxml)
spend most of their CPU time descending into children you do not need.

crxml skips the nesting:
- The **stream engine** walks the XML once with quick-xml and yields flat dicts.
- The **parallel engine** memory-maps the file, splits it at row boundaries,
  and parses each chunk on its own thread into Arrow buffers directly (no dicts).
- Pipeline stages that rename, cast, drop, or filter fields execute in the Rust
  parse loop, before any Python object is created.

---

## Comparison: stream vs parallel

| Task | stream | parallel |
|---|---|---|
| Row iteration | Yields dicts lazily | Arrow table first, then dicts (slower) |
| DataFrame output | Collects dicts, converts | Direct Arrow buffers, zero-copy |
| 100 MB synthetic | 2.3 s | **0.21 s** |
| 533 MB real export | 12.8 s | **1.13 s** |
| Peak RSS | ~1.07 GB | ~534 MB (file size, mmap) |
| Pipeline fusion | No (dict path) | Yes (Rust BuildPlan) |

The stream engine materializes one dict per row: fully consuming a large
file costs roughly 10x its size in memory (~1 GB RSS for a 100 MB file).
Use it for incremental processing; use table sinks for collection.

For files larger than RAM, add `memory="500MB"` to any engine for bounded mode:
peak RSS tracks the budget, not the file.

[Full benchmark details](docs/performance.md)

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

## Engine guide

| Engine | When to use |
|---|---|
| `stream` | Row-by-row iteration (for row in source) |
| `columnar` | Single-threaded Arrow output |
| `parallel` | Fastest DataFrame output (default for files > 8 MB) |
| `bounded` | Files larger than RAM (`memory="500MB"` with any engine) |

Pass `engine=` explicitly, or let `auto` select the best engine per call.

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
