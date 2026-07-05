# Pipeline Fusion

Pipeline fusion compresses multiple transformation stages into a single
execution pass, reducing Python overhead and memory allocations.

## Three levels of fusion

crxml has three fusion mechanisms:

| Level | Mechanism | When it applies |
|-------|-----------|-----------------|
| Dict-level fusion | `apply` + `__call__` protocol | Any pipeline with fusable stages |
| Columnar fusion | `_plan_kwargs` into Rust `BuildPlan` | Source supports columnar engine, stages export a plan |
| Vectorized batch chain | Volcano-style pull on Arrow `RecordBatch` | After columnar fusion, remaining stages implement `_plan_kwargs` |

A stage is **fusable** if it has both `apply(self, record) -> dict | None` and
`__call__(self, stream)`. When a contiguous run of fusable stages exists at
the front of the pipeline, they are fused into a single tight loop that
avoids Python generator overhead.

A stage supports **columnar fusion** if it implements
`_plan_kwargs(self) -> dict | None`. When all stages in a pipeline are
columnar-fusable, the entire pipeline compiles into the Rust columnar engine
and no Python dicts are created until the final Arrow table is converted.

## Decision tree

When you iterate a pipeline, `fusion.py` follows this logic:

1. **Try columnar fusion**: if the source has a `_read_arrow` method and
   stages export `_plan_kwargs`, the pipeline runs entirely in Rust.
2. **Vectorized batch chain**: if columnar fusion found pushdown stages but
   remaining stages exist, they run on Arrow `RecordBatch` objects via the
   batchpipe engine, keeping data in columnar format and avoiding per-row
   Python dict construction.
3. **Dict-level fusion**: if columnar fusion is not possible, the first
   contiguous run of fusable stages is fused into a single loop.
4. **Sequential**: remaining stages run as Python generators on the dict
   stream.

## When fusion applies

| Pipeline | Fusion level | Performance |
|----------|-------------|-------------|
| `Source \| RenameFields \| CastTypes` | Columnar | Fastest (all Rust) |
| `Source \| DropFields \| RenameFields` | Columnar | Fastest (all Rust) |
| `Source \| FilterRows(field=..., op=..., value=...)` | Columnar | Fastest (all Rust) |
| `Source \| custom_fusable_stage \| CastTypes` | Dict-level | Fast (no generator overhead) |
| `Source \| generator_func \| CastTypes` | Sequential | Fusable stages after generator are NOT fused |
| `Source \| CastTypes \| generator_func` | Columnar + dict tail | Columnar up to the generator, then dicts |

For optimal performance, place fusable stages at the front of the pipeline:

```python
# Good: CastTypes and DropFields fuse into columnar plan
pipe = source | CastTypes({"amt": float}) | DropFields(["tmp"]) | custom_filter

# Less good: custom_filter breaks the fusable chain
pipe = source | custom_filter | CastTypes({"amt": float}) | DropFields(["tmp"])
```

## How columnar fusion works

1. The pipeline calls `_try_columnar_fusion(source, stages)`.
2. For each stage, `_plan_kwargs()` is called. If it returns a dict, the
   kwargs are merged into a single `plan_overrides` dict and the stage is
   skipped in the Python stage list.
3. `source._read_arrow(plan_overrides=plan_overrides)` is called. This runs
   the Rust columnar engine with the fused plan, producing a `pyarrow.Table`
   directly from the XML.
4. The Arrow table is wrapped in a row-by-row dict iterator.
5. Any remaining stages (those that did not provide `_plan_kwargs`) run on the
   dict stream.

This means columnar fusion can be partial. A pipeline like:

```python
source | RenameFields({...}) | CastTypes({...}) | custom_lambda
```

will execute `RenameFields` and `CastTypes` in Rust, produce an Arrow table,
convert to dicts, then apply `custom_lambda` to each dict. No unnecessary
Python object creation happens for the fused stages.

## How dict-level fusion works

1. The pipeline scans stages from the front until it finds a non-fusable
   stage (no `apply` method).
2. All fusable stages are combined into a single `fused()` generator:

```python
def fused():
    for record in source:
        r = record
        for fn in bound_stage_applies:
            r = fn(r)
            if r is None:
                break
        else:
            yield r
```

3. Non-fusable stages wrap the fused generator.

## Verifying fusion

Set logging to DEBUG to see fusion decisions:

```python
import logging
logging.basicConfig(level=logging.DEBUG)

# Logs: "columnar fusion with overrides: ..." or "fused N stages"
```

Or check programmatically by inspecting the pipeline:

```python
pipe = source | CastTypes({"x": float})
print(type(pipe._stages[0]))  # <class 'crxml.stages.cast.CastTypes'>
```

If the pipeline uses the columnar engine, `source._read_arrow` is called
internally and the Rust-side profile counters show the fused plan.

## Performance comparison

Using a 100 MB file with a 4-stage pipeline:

| Pipeline | Time | Speedup vs sequential |
|----------|------|----------------------|
| Sequential (no fusion) | 2.27s | 1x |
| Dict-level fusion only | 1.89s | 1.2x |
| Full columnar fusion | 0.69s | 3.3x |

Columnar fusion is particularly effective because it skips the two most
expensive operations in the stream path: HTML unescaping and Python dict
construction.
