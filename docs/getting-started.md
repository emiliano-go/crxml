# Getting Started

This guide walks through a complete round-trip: installation, first parse,
schema inspection, a simple pipeline, and DataFrame conversion.

## Install

```bash
pip install crxml
```

See [Installation](installation.md) for details on building from source
and platform support.

## Your first source

Create a small Crystal Reports XML file and point `CrystalXMLSource` at it:

```python
from crxml import CrystalXMLSource

src = CrystalXMLSource("report.xml")

for row in src:
    print(row)
```

Each `row` is a `dict[str, str]`. The keys are field names from the CR XML
(e.g. `{Report.FieldName}`) and the values are the raw text content.

## Inspect the schema

Use `.schema()` to see the fields without consuming the stream:

```python
src = CrystalXMLSource("report.xml")
fields = src.schema()  # list of field name strings
```

This is useful for building dynamic pipelines.

## Schema for performance

`.schema()` returns field names for inspection. When you know the fields
upfront, pass them as `schema=` to skip discovery and hit the fast path:

```python
schema = src.schema()  # discover once
fast = CrystalXMLSource("report.xml", row_tag="Details", schema=schema)
df = fast.to_dataframe()  # no discovery pass
```

This is the single largest performance lever for bounded/streaming mode.
On production data, explicit schema lifts throughput from 4.2 GB/s to
7.6 GB/s on a 533 MB report. See [Performance](performance.md) for
benchmarks.

## Simple pipeline

The `|` operator chains transformation stages. Nothing executes until you
iterate or sink the result:

```python
from crxml import CrystalXMLSource, RenameFields, CastTypes, DropFields

pipe = (
    CrystalXMLSource("report.xml")
    | RenameFields({
        "{Report.InvoiceNo}": "invoice",
        "{Report.Customer}": "customer",
        "{Report.Amount}": "amount",
    })
    | CastTypes({"amount": float})
    | DropFields(["{Report.TaxRate}"])
)

for row in pipe:
    print(row["invoice"], row["amount"])
```

## Convert to DataFrame

```python
from crxml import to_dataframe

df = to_dataframe(pipe)
```

This collects all rows into a pandas DataFrame. For large files use
`chunksize=` to build the DataFrame incrementally (see [Sinks](usage/sinks.md)).

## Next steps

- [Usage guide](usage/basic.md), deeper topics: custom stages, parallel mode, branching
- [Pipeline API](usage/pipeline.md), how `|` and lazy evaluation work
- [Built-in stages](usage/stages.md), reference for all four stage types
- [Performance](performance.md), benchmarks, memory model, bottlenecks
