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
fields = src.schema()  # list of (key, sample_value) tuples
for key, sample in fields:
    print(f"{key}: {sample!r}")
```

This is useful for building dynamic pipelines.

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
    | DropFields("{Report.TaxRate}")
)

for row in pipe:
    print(row["invoice"], row["amount"])
```

## Convert to DataFrame

```python
from crxml import to_dataframe

df = pipe |> to_dataframe
```

This collects all rows into a pandas DataFrame. For large files use
`chunksize=` to build the DataFrame incrementally (see [Sinks](usage/sinks.md)).

## Next steps

- [Usage guide](usage/basic.md), deeper topics: custom stages, parallel mode, branching
- [Pipeline API](usage/pipeline.md), how `|` and lazy evaluation work
- [Built-in stages](usage/stages.md), reference for all four stage types
- [Performance](performance.md), benchmarks, memory model, bottlenecks
