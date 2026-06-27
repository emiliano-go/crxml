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
processes 100 MB in ~0.42 seconds using ~75 MB RSS for streaming.

This library is conceptually based on [carlosplanchon/xmlstreamer](https://github.com/carlosplanchon/xmlstreamer).

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

- **RenameFields**(mapping), renames dict keys
- **CastTypes**(types, errors="raise"), casts fields to target types
- **DropFields**(*fields), removes fields from rows
- **FilterRows**(predicate), keeps rows matching predicate

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

| Test | Size | Rows | Time | Rows/s | MB/s | RSS |
|------|------|------|------|--------|------|-----|
| Stream | 10 MB | 9,010 | 0.043s | 211 K | 234 | 22 MB |
| Stream | 50 MB | 45,328 | 0.223s | 203 K | 224 | 45 MB |
| Stream | 100 MB | 90,384 | 0.418s | 216 K | 239 | 75 MB |
| To list | 10 MB | 9,010 | 0.052s | 174 K | 192 | 32 MB |
| To list | 50 MB | 45,328 | 0.249s | 182 K | 201 | 98 MB |
| To list | 100 MB | 90,384 | 0.478s | 189 K | 209 | 181 MB |
| Pipeline | 10 MB | 9,010 | 0.060s | 150 K | 166 | 32 MB |
| Pipeline | 50 MB | 45,328 | 0.295s | 154 K | 169 | 96 MB |
| Pipeline | 100 MB | 90,384 | 0.579s | 156 K | 173 | 176 MB |
| DataFrame | 10 MB | 9,010 | 0.320s | 28 K | 31 | 86 MB |
| DataFrame | 50 MB | 45,328 | 0.538s | 84 K | 93 | 152 MB |
| DataFrame | 100 MB | 90,384 | 0.829s | 109 K | 121 | 234 MB |

pandas is imported lazily — memory climbs only when `to_dataframe` is called.

## Publishing

```bash
./upload.sh
```

Builds a manylinux2014 wheel + sdist and uploads to PyPI. Requires `maturin` and `twine`. The `--manylinux 2014 --zig` flag ensures PyPI-compatible platform tags — `python -m build` does not support manylinux flags via PEP 517.

## Documentation

Full documentation is available at the [project site](https://crxml.emiliano-go.com/),
covering installation, usage, stages, custom stages, architecture, performance,
FastAPI integration, and the Rust core.

## License

MIT
