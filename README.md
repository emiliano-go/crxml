# crxml

Fast streaming parser for Crystal Reports XML exports.

```python
from crxml import CrystalXMLSource, to_dataframe

df = to_dataframe(CrystalXMLSource("report.xml", row_tag="Details"))
print(df.head())
```

## Installation

**Prerequisites:** Python ≥3.10 and [Rust](https://rustup.rs).

```bash
pip install crxml
```

## About

crxml streams through Crystal Reports XML files row by row, never loading
the full document into memory. It extracts field data from nested CR field
elements and yields flat dictionaries. A built-in pipeline lets you rename,
cast, filter, and drop fields with `|` operators. The Rust backend
processes 100 MB in ~0.5 seconds using <100 MB RSS.

## API

### CrystalXMLSource

```python
CrystalXMLSource(source, row_tag="Details")
```

Parses a CR XML file and yields `dict[str, str]` rows. Accepts a file path
(string or `Path`), or a file-like object with a `.name` attribute. The
`row_tag` parameter controls which XML element is treated as a record
(default: `Details`).

### Pipeline stages

Stages are chained with `|`:

```python
from crxml.stages import RenameFields, CastTypes, DropFields, FilterRows

pipeline = (
    CrystalXMLSource("report.xml")
    | RenameFields({"f1": "invoice", "f2": "amount"})
    | CastTypes({"amount": float})
    | DropFields("tax_rate")
    | FilterRows(lambda r: r["amount"] > 100)
)
```

- **RenameFields**(mapping) — renames dict keys
- **CastTypes**(types, errors="raise") — casts fields to target types
- **DropFields**(*fields) — removes fields from rows
- **FilterRows**(predicate) — keeps rows matching predicate

### Sinks

```python
from crxml import to_dataframe, to_csv, collect

df = to_dataframe(pipeline)                  # → pd.DataFrame
to_csv(pipeline, "out.csv")                  # → CSV file
rows = collect(pipeline)                     # → list[dict]
```

### Parallel mode

```python
df = pipeline.parallel(workers=4) |> to_dataframe
```

Distributes batches across worker processes. See the docs for requirements.

## Benchmarks

| Test | Size | Rows | Time | Throughput | RSS |
|------|------|------|------|------------|-----|
| Stream | 10 MB | 9,010 | 0.058s | 155 K rows/s | 94 MB |
| Stream | 50 MB | 45,328 | 0.261s | 174 K rows/s | 94 MB |
| Stream | 100 MB | 90,384 | 0.508s | 178 K rows/s | 94 MB |
| To list | 100 MB | 90,384 | 0.574s | 157 K rows/s | 339 MB |
| Pipeline | 100 MB | 90,384 | 0.781s | 116 K rows/s | 327 MB |
| DataFrame | 100 MB | 90,384 | 0.675s | 134 K rows/s | 351 MB |

RSS stays flat at ~94 MB regardless of file size.

## Documentation

Full documentation is available at the [project site](https://emiliano-gandini-outeda.me/crxml/),
covering installation, usage, stages, custom stages, architecture, performance,
FastAPI integration, and the Rust core.

## License

MIT
