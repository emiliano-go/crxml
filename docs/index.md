# crxml

High-performance Crystal Reports XML to Arrow/DataFrame engine for Python.
Parse, filter, rename, cast, and project Crystal Reports XML directly into
columnar data, with Rust execution, parallel parsing, bounded-memory
processing, and automatic query fusion. Powered by
[rypipe](https://github.com/emiliano-go/rypipe).

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
