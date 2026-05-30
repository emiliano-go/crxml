# crxml

Fast streaming parser for SAP Crystal Reports XML exports.
Parse 100 MB in 0.6 seconds. Constant memory.

## Features

- Streaming: never loads the full file into memory
- Fast: Rust parser via PyO3 + quick-xml
- Pipeline API: compose transformations with `|`
- Parallel mode: multi-core batch processing
- Pandas-native: direct to DataFrame or CSV

## Quick Example

```python
from crxml import CrystalXMLSource, RenameFields, CastTypes, to_dataframe

df = (
    CrystalXMLSource("report.xml")
    | RenameFields({"f1": "invoice", "f2": "amount"})
    | CastTypes({"amount": float})
    |> to_dataframe
)
```

## License

MIT
