# crxml

High-performance Crystal Reports XML to Arrow/DataFrame engine for Python.
Parse, filter, rename, cast, and project Crystal Reports XML directly into
columnar data, with Rust execution, parallel parsing, bounded-memory
processing, and automatic query fusion.

> **Origin story:** `crxml` was built first as a standalone Crystal Reports XML parser. Its engine: row splitting (`CrystalXmlSplitter`), field extraction (`CrystalXmlDecoder` via `memchr` scanner), typed builders (`TableBuilder` + `ExecutionPlan` pushdown), and parallel/bounded drivers: proved fast enough (up to **4.1 GB/s** on uniform exports and **4.2 GB/s** on high-cardinality production reports) to be useful for any format. That engine was then separated and abstracted into [`rypipe`](https://github.com/emiliano-go/rypipe) (`rypipe-core` + `rypipe-python`). `crxml` now lives as a thin adapter (`crxml-core` `src/crxml_core/src/xml/`) on top of `rypipe`.

## Features

- Streaming: never loads the full file into memory
- Fast: Rust parser via PyO3 + memchr scanner
- Pipeline API: compose transformations with `|`
- Parallel mode: multi-core batch processing
- Pandas-native: direct to DataFrame or CSV

## Quick Example

```python
from crxml import CrystalXMLSource, RenameFields, CastTypes, to_dataframe

pipe = (
    CrystalXMLSource("report.xml")
    | RenameFields({"f1": "invoice", "f2": "amount"})
    | CastTypes({"amount": float})
)
df = to_dataframe(pipe)
```

## License

MIT
