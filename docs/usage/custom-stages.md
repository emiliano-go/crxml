# Custom Stages

There are three styles for writing custom pipeline stages.

## Generator style

A generator function that yields transformed rows:

```python
def uppercase_names(stream):
    for row in stream:
        if "name" in row:
            row["name"] = row["name"].upper()
        yield row
```

Usage:

```python
pipe = CrystalXMLSource("report.xml") | uppercase_names
```

## Map style

A function that returns a map iterator:

```python
def strip_whitespace(stream):
    return map(lambda r: {k: v.strip() for k, v in r.items()}, stream)
```

## Fusable protocol

For optimal performance (especially in parallel mode), implement a class with
both `apply` and `__call__`:

```python
class MultiplyField:
    def __init__(self, field: str, factor: float):
        self.field = field
        self.factor = factor

    def apply(self, record: dict) -> dict | None:
        if self.field in record:
            try:
                record[self.field] = float(record[self.field]) * self.factor
            except (ValueError, TypeError):
                pass
        return record

    def __call__(self, stream):
        for row in stream:
            yield self.apply(row)
```

When a stage implements the `Fusable` protocol (has `apply` and `__call__`),
the pipeline can fuse a contiguous run of fusable stages into a single tight
loop, avoiding Python generator overhead.

## When to use each style

| Style | Best for | Parallel? |
|-------|----------|-----------|
| Generator | Simplicity, complex logic | No (captures self/closures) |
| Map | Simple transforms | No (lambda not picklable) |
| Fusable | Performance, parallel mode | Yes |

## Picklability for parallel mode

To use a custom stage with `.parallel()`, it must be picklable:

- Top-level module functions only (no lambdas)
- Classes with `__init__` storing simple data
- No closures or local functions

crxml validates picklability at pipeline construction time and raises
`TypeError` for incompatible stages.

## Columnar plan fusion

For maximum performance, a stage can implement `_plan_kwargs(self) -> dict | None`.
When this method returns a dict, the stage is compiled into the engine's
`ExecutionPlan` and runs during XML parsing, before any Python dict is
created. This bypasses Python entirely for that stage.

Built-in stages that support columnar plan fusion:

| Stage | `_plan_kwargs` effect |
|-------|----------------------|
| `RenameFields` | Adds `field_mapping` to the plan |
| `CastTypes` | Adds `field_types` to the plan |
| `DropFields` | Adds `drop_fields` to the plan |
| `FilterRows` | Adds `filter` to the plan (constant/column predicates only) |

Example of a custom stage that fuses into the columnar plan:

```python
class DropFieldsIfEmpty:
    def __init__(self, fields: list[str]):
        self.fields = fields

    def apply(self, record: dict) -> dict | None:
        for f in self.fields:
            record.pop(f, None)
        return record

    def __call__(self, stream):
        for row in stream:
            yield self.apply(row)

    def _plan_kwargs(self) -> dict | None:
        return {"drop_fields": self.fields}
```

Notes:

- `_plan_kwargs` is only called by `CrystalXMLSource` objects that support the
  columnar engine.
- If `_plan_kwargs` returns `None`, the stage is treated as a regular fusable
  stage (dict-level fusion).
- Non-fusable stages in the pipeline are always applied as Python generators
  on the dict stream after columnar fusion completes.
