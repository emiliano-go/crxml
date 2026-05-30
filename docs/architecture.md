# Architecture

## Overview

```
source ──► stages ──► sink
```

A `CrystalXMLSource` reads the CR XML file and yields rows. Stages transform
rows via the `|` operator. Sinks consume the pipeline and materialize results.

## Rust core (`crxml_core`)

The Rust crate at `src/crxml_core/` uses:

- **quick-xml** — fast, streaming XML reader
- **PyO3** — Python bindings
- **Buffer reuse** — `Vec<u8>` is reused across rows to minimize allocations

The reader:

1. Walks the XML tree to find `<Details>` elements (or custom `row_tag`).
2. For each `<Details>`, extracts `<Field>` / `<Text>` children.
3. Gets the key from `FieldName` attribute (or nested `<FieldName>` element).
4. Gets the value from the first `<FormattedValue>` or `<Value>` child.
5. Yields the row as a `PyDict`.

### Namespace handling

CR XML uses `urn:crystal-reports:schemas:report-detail`. The Rust parser
strips namespace prefixes via local-name matching on the XML reader.

## Python source layer

`CrystalXMLSource` is a thin Python wrapper:

```python
class CrystalXMLSource:
    def __init__(self, source, row_tag="Details"):
        ...

    def __iter__(self):
        if _RUST:
            return _rust_iter(self.source, self.row_tag)
        return _lxml_iter(self.source, self.row_tag)
```

The `_RUST` flag is set at import time. The lxml fallback uses the same
nested `<Field>` / `<Text>` extraction logic but via lxml's iterparse.

## Pipeline

`Pipeline` is an immutable wrapper:

- `__or__` creates a new `Pipeline` with the stage appended.
- `__iter__` decides execution strategy:
  - **Layer B (fusion):** if all stages are Fusable, runs a single tight loop
  - **Layer C (prefetch):** producer thread + consumer thread with bounded queue
  - **Layer D (parallel):** `ProcessPoolExecutor` with batch dispatch

### Fusion (Layer B)

A contiguous run of fusable stages (implementing `apply` + `__call__`) is
fused into a single inner loop. This avoids Python generator overhead:

```python
for row in source:
    for stage in fused_stages:
        row = stage.apply(row)
        if row is None:
            break
    if row is not None:
        yield row
```

### Prefetch (Layer C)

A background thread reads the source and pushes rows into a bounded queue.
The main thread reads from the queue and runs stages. This overlaps I/O with
transformation work.

### Parallel (Layer D)

A reader thread produces batches. Each batch is sent to a worker process via
`ProcessPoolExecutor`. Workers run the fused pipeline. Results are collected
in order via `concurrent.futures`.

## Error handling

- Parse errors: raised immediately during iteration
- Type errors: controlled by `CastTypes(errors=...)`
- Picklability validation: raised at `parallel()` call time
