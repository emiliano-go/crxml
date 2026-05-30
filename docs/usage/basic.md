# Basic Parsing

## Opening a file

`CrystalXMLSource` accepts a path string, a `pathlib.Path` object, or any
file-like object with a `.name` attribute:

```python
from crxml import CrystalXMLSource

# path string
src = CrystalXMLSource("report.xml")

# pathlib.Path
from pathlib import Path
src = CrystalXMLSource(Path("report.xml"))

# file-like object
with open("report.xml") as f:
    src = CrystalXMLSource(f)
```

### Parameters

| Param    | Type                          | Default         | Description                       |
|----------|-------------------------------|-----------------|-----------------------------------|
| `source` | `str \| Path \| TextIOBase`   | —               | Path or file-like object          |
| `row_tag`| `str`                         | `"Details"`     | XML tag for each record row       |

The `row_tag` parameter lets you target a different repeating element if your
CR XML uses a non-standard tag name.

## Iteration

`CrystalXMLSource` is iterable. Each row is a `dict[str, str]`:

```python
for row in CrystalXMLSource("report.xml"):
    print(row["{Report.InvoiceNo}"], row["{Report.Amount}"])
```

Keys are the `FieldName` attribute values from the CR XML (e.g.
`{Report.InvoiceNo}`). Values are the raw text of the first
`<FormattedValue>` or `<Value>` child element.

## Schema inspection

Call `.schema()` to discover fields without consuming the stream:

```python
src = CrystalXMLSource("report.xml")
fields = src.schema()  # list of (key, sample_value) tuples
```

The source yields rows internally and caches them, so the first batch is not
lost. `.schema()` is safe to call before building a pipeline.

## Memory model

The parser streams the file in constant memory. The Rust backend reuses
internal buffers across rows and never materializes the full document.
RSS stays flat regardless of file size (e.g. ~94 MB for both 10 MB and
100 MB inputs).

## CR XML layout detection

Crystal Reports XML stores field values in two patterns:

- **Attribute style:** `<Field FieldName="{Report.Amount}"><Value>123.45</Value></Field>`
- **Element style:** `<Field><FieldName>{Report.Amount}</FieldName><Value>123.45</Value></Field>`
- **Mixed:** some fields use attributes, others use child elements

The parser detects both styles automatically — no configuration needed.
