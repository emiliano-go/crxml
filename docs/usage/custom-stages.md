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
`UnpicklableStageError` for incompatible stages.
