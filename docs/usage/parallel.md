# Parallel Execution

For large files, `.parallel()` splits work across multiple processes.

## Usage

```python
from crxml import CrystalXMLSource, RenameFields, CastTypes, to_dataframe

pipe = (
    CrystalXMLSource("report.xml")
    | RenameFields({"f1": "name", "f2": "total"})
    | CastTypes({"total": float})
)

df = to_dataframe(pipe.parallel(workers=4, batch_size=5000))
```

## Parameters

| Param        | Type  | Default | Description                            |
|--------------|-------|---------|----------------------------------------|
| `workers`    | `int` | `None`  | Number of worker processes (CPU count) |
| `batch_size` | `int` | `1000` | Rows per batch sent to workers         |

## Requirements

- All stages in the pipeline must be **fusable** (implement `apply` + `__call__`)
- All stages must be **picklable** (no lambdas, no closures)
- The source must be iterable multiple times (file re-opened per batch)

## How it works

1. A reader thread reads the source and splits rows into batches.
2. Batches are dispatched to a `ProcessPoolExecutor`.
3. Each worker runs the fused pipeline on its batch.
4. Results are returned in order via futures.

## When to use

Parallel mode adds overhead for batch serialization and IPC. The heuristic:

| File size  | Recommended |
|------------|-------------|
| < 50 MB    | Sequential  |
| 50 MB to 200 MB  | Recommended |
| > 200 MB   | Parallel    |

## Validation

crxml validates all stages at pipeline construction:

```python
from crxml import CrystalXMLSource, RenameFields, FilterRows

pipe = CrystalXMLSource("report.xml") | RenameFields({"a": "b"})
# This works, both stages are Fusable and picklable

pipe2 = CrystalXMLSource("report.xml") | FilterRows(lambda r: r["x"] > 1)
pipe2.parallel()  # raises TypeError, lambda not picklable
```

## Named function example

```python
def above_threshold(row):
    return row if float(row.get("amount", 0)) > 100 else None

pipe = CrystalXMLSource("report.xml") | above_threshold
pipe.parallel(workers=2)  # works, module-level function
```
