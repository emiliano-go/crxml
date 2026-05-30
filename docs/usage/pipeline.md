# Pipeline API

The `|` operator composes transformation stages into a lazy pipeline.

## How it works

`CrystalXMLSource | stage` returns a `Pipeline` object. Chaining multiple
stages creates a composition; nothing executes until you iterate or sink the
result.

```python
from crxml import CrystalXMLSource, RenameFields, CastTypes

pipe = (
    CrystalXMLSource("report.xml")
    | RenameFields({"f1": "name", "f2": "total"})
    | CastTypes({"total": float})
)

# No iteration has happened yet
for row in pipe:  # execution starts here
    print(row)
```

## Immutable composition

Pipelines are immutable. Every `|` produces a new `Pipeline` object without
modifying the previous one:

```python
base = CrystalXMLSource("report.xml") | RenameFields(mapping)

# These are independent — each re-reads the source
pipe_a = base | CastTypes({"amount": float})
pipe_b = base | DropFields("tax_rate")
```

## Pipeline object

Usually created implicitly via `|`. The `Pipeline` class is also importable:

```python
from crxml import Pipeline, CrystalXMLSource, RenameFields

pipe = Pipeline(CrystalXMLSource("report.xml"), RenameFields(mapping))
```

## Lazy evaluation

Pipelines are fully lazy until one of:

- `for row in pipeline:` — per-row iteration
- `list(pipeline)` — collect all rows
- `to_dataframe(pipeline)` — DataFrame sink
- `to_csv(pipeline, path)` — CSV sink
- `collect(pipeline)` — list sink

## Example: 3-stage pipeline

```python
pipe = (
    CrystalXMLSource("report.xml")
    | RenameFields({"vendor": "supplier", "price": "cost"})
    | CastTypes({"cost": float})
    | FilterRows(lambda r: r["cost"] > 100)
)
```

Each stage processes rows in sequence. A row dropped by `FilterRows` never
reaches later stages.
