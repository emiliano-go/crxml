# crxml — spec

Crystal Reports XML → pandas DataFrame pipeline library with Unix-style pipe composition.

---

## Background

SAP Crystal Reports can export report data as XML. That XML has a proprietary schema — `<Report>`, `<Details>`, `<Group>` sections, fields exposed either as element attributes or as child elements depending on the CR version — that no generic XML-to-DataFrame library handles correctly.

No Python library exists that targets this specific format. The closest analogues are `sspyrs2` (same idea, but for SSRS) and generic `ElementTree`/`lxml` snippets that require manual schema knowledge.

`crxml` fills that gap: it wraps `xmlstreamer` (by carlosplanchon) as the streaming backend, exposes the CR XML structure as a lazy `Iterable[dict]`, and lets the user compose transformation stages using the `|` operator before materializing into a `DataFrame` or CSV.

---

## Goals

- Parse Crystal Reports XML exports into pandas DataFrames or CSV files.
- Stream the XML record-by-record — never load the entire file into memory.
- Compose transformations as a Unix-style pipeline using `|`.
- Expose a `.schema()` method to inspect field names before consuming the stream.
- Ship useful built-in stages: rename, cast, filter, drop.
- Support custom stages as any callable matching the stage signature.
- Maximize throughput on large reports via stage fusion, prefetch buffering, and parallel batch processing.
- Keep dependencies minimal: `xmlstreamer`, `pandas`, stdlib only.

## Non-goals

- Parsing `.rpt` binary files.
- Connecting to a live Crystal Reports or BOE server.
- Rendering or formatting report output.
- Writing back to CR XML format.

---

## Core concepts

### Stage

A stage is any callable with the signature:

```python
Callable[[Iterable[dict]], Iterable[dict]]
```

Stages are lazy: they wrap the upstream iterable and yield transformed records downstream. A stage can be:

- A generator function (uses `yield`).
- A function that returns `map(...)`, `filter(...)`, or any other iterable.
- An instance of a built-in stage class (which implements `__call__`).
- A plain lambda, provided it returns an iterable.

All built-in stages additionally implement the `Fusable` protocol (see Pipeline internals), which enables loop fusion.

### Pipeline

A `Pipeline` holds a source and an ordered list of stages. It is itself an `Iterable[dict]` — iterating it runs the full chain lazily.

The `|` operator appends a stage and returns a new `Pipeline` (immutable composition).

Internally, `Pipeline` applies up to four optimization layers depending on the stages present and the execution mode requested. See **Pipeline internals** for the full model.

### Source

`CrystalXMLSource` wraps `xmlstreamer` and emits one `dict` per data row. It handles the two CR XML layouts (attribute-based vs element-based fields) transparently.

### Sink

A sink materializes the pipeline into a concrete output. Sinks are regular functions that accept a `Pipeline` (or any `Iterable[dict]`) and produce a side effect or return value.

---

## Package layout

```
crxml/
├── __init__.py        # public API exports
├── source.py          # CrystalXMLSource
├── pipeline.py        # Pipeline class + optimization layers
├── fusion.py          # Fusable protocol + fused loop builder
├── parallel.py        # prefetch thread + ProcessPoolExecutor worker
├── stages/
│   ├── __init__.py    # re-exports all built-in stages
│   ├── rename.py      # RenameFields
│   ├── cast.py        # CastTypes
│   ├── filter.py      # FilterRows
│   └── drop.py        # DropFields
└── sinks.py           # to_dataframe, to_csv, collect
```

---

## API

### `CrystalXMLSource`

```python
class CrystalXMLSource:
    def __init__(self, path: str | PathLike) -> None: ...
    def schema(self) -> list[str]: ...
    def __iter__(self) -> Iterator[dict]: ...
    def __or__(self, stage: Stage) -> Pipeline: ...
```

**`schema()`**

Reads only the first data record from the XML and returns the list of field names, without advancing or consuming the main stream. Useful for inspecting an unfamiliar report before building a pipeline.

```python
src = CrystalXMLSource("report.xml")
print(src.schema())
# ['invoice_no', 'client_name', 'amount', 'issue_date']
```

**CR XML layout detection**

Crystal Reports XML varies across export versions:

- **Attribute layout**: fields are XML attributes on the row element.
  `<Row invoice_no="1001" client_name="Acme" amount="500.00"/>`
- **Element layout**: fields are child elements of the row element.
  `<Row><invoice_no>1001</invoice_no><client_name>Acme</client_name></Row>`

`CrystalXMLSource` detects which layout is in use by inspecting the first row and normalizes both into a flat `dict` before yielding. Mixed layouts (some fields as attributes, some as children) are also handled.

---

### `Pipeline`

```python
class Pipeline:
    def __init__(
        self,
        source: Iterable[dict],
        stages: list[Stage] | None = None,
        *,
        batch_size: int = 1_000,
        prefetch: bool = True,
        workers: int | None = None,
    ) -> None: ...

    def __or__(self, stage: Stage) -> "Pipeline": ...
    def __iter__(self) -> Iterator[dict]: ...
    def parallel(self, workers: int | None = None, batch_size: int = 1_000) -> "Pipeline": ...
```

`Pipeline` is immutable: `|` always returns a new instance. The original pipeline is not modified.

**Parameters:**

- `batch_size`: number of records per internal batch. Used by the prefetch buffer and the parallel executor. Default `1_000`.
- `prefetch`: if `True`, a background thread reads the source into a bounded `queue.Queue` while the main thread processes. Default `True`.
- `workers`: number of worker processes for parallel batch execution. `None` means single-process (optimization layers A–C only). Set explicitly or via `.parallel()` to activate layer D.

**`.parallel(workers, batch_size)`** is a convenience method that returns a new `Pipeline` with `workers` set, activating `ProcessPoolExecutor` dispatch. Equivalent to constructing with `workers=N`.

```python
# activate parallel execution with 4 workers
pipeline = (
    src
    | RenameFields({...})
    | CastTypes({...})
    | FilterRows(lambda r: r["amount"] > 0)
).parallel(workers=4, batch_size=2_000)

df = to_dataframe(pipeline, chunksize=20_000)
```

---

## Pipeline internals

This is the core of the library. Four optimization layers are applied in order, each building on the previous.

### Layer A — pure lazy generators (baseline)

The naive implementation chains generators: each `next()` call on the sink cascades through every stage. With N stages and M records, this is `N × M` Python function calls and `N × M` generator frame resumptions.

```
sink.next()
  └─ FilterRows.__next__()        # Python frame
       └─ CastTypes.__next__()    # Python frame
            └─ RenameFields.__next__()  # Python frame
                 └─ source.__next__()   # xmlstreamer C frame
```

This is correct and memory-efficient but has high per-record overhead when N is large.

### Layer B — stage fusion

All built-in stages implement the `Fusable` protocol:

```python
class Fusable(Protocol):
    def apply(self, record: dict) -> dict | None:
        """
        Transform one record. Return the transformed dict, or None to drop the record.
        Must be a pure function with no dependency on stream state.
        """
        ...
```

When `Pipeline.__iter__` is called and all stages are `Fusable`, it skips the generator chain entirely and runs a single fused loop:

```python
def _fused_iter(source: Iterable[dict], stages: list[Fusable]) -> Iterator[dict]:
    for raw in source:
        record = raw
        for stage in stages:
            record = stage.apply(record)
            if record is None:
                break          # FilterRows dropped it
        else:
            yield record
```

This collapses N generator frames into a single Python loop regardless of how many stages are composed. The `itertools`-based built-ins (`filter`, `map`) running at the C layer further reduce interpreter overhead.

**Fusable detection at composition time:**

```python
# Pipeline.__or__
def __or__(self, stage: Stage) -> "Pipeline":
    new_stages = self._stages + [stage]
    all_fusable = all(isinstance(s, Fusable) for s in new_stages)
    return Pipeline(self._source, new_stages, _all_fusable=all_fusable, ...)
```

If a custom (non-`Fusable`) stage is added, the pipeline falls back to the chained generator model for that stage and beyond. Fusion is applied only to the leading contiguous run of `Fusable` stages.

```python
# [RenameFields, CastTypes, FilterRows, custom_fn, DropFields]
#  ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ fused     ^ generator
```

### Layer C — prefetch buffer (I/O / CPU overlap)

The XML parser (`xmlstreamer`) is I/O-bound. The fused stage loop is CPU-bound. Without prefetch, the CPU sits idle while the parser reads the next record.

A background `threading.Thread` reads the source into a `queue.Queue[list[dict]]` in batches of `batch_size`, running concurrently with the main thread's processing loop. The GIL is not a bottleneck here because `xmlstreamer`'s C extension releases it during I/O.

```
┌─────────────────────┐      queue.Queue        ┌──────────────────────┐
│  Reader thread       │  ──── batch ────►       │  Main thread          │
│  xmlstreamer (C)     │                         │  fused stage loop     │
│  releases GIL on I/O │  ◄── sentinel ────      │  yields records       │
└─────────────────────┘                         └──────────────────────┘
```

Implementation:

```python
import threading
import queue
from typing import Iterator

_SENTINEL = object()

def _prefetch_reader(
    source: Iterable[dict],
    q: queue.Queue,
    batch_size: int,
) -> None:
    batch = []
    try:
        for record in source:
            batch.append(record)
            if len(batch) >= batch_size:
                q.put(batch)
                batch = []
        if batch:
            q.put(batch)
    finally:
        q.put(_SENTINEL)


def _prefetch_iter(
    source: Iterable[dict],
    batch_size: int,
    maxsize: int = 8,
) -> Iterator[dict]:
    q: queue.Queue = queue.Queue(maxsize=maxsize)
    t = threading.Thread(target=_prefetch_reader, args=(source, q, batch_size), daemon=True)
    t.start()
    while True:
        item = q.get()
        if item is _SENTINEL:
            break
        yield from item
    t.join()
```

`maxsize=8` caps the prefetch queue at 8 batches, bounding memory: at `batch_size=1_000` that is at most 8 000 records buffered ahead, regardless of file size.

### Layer D — parallel batch processing with `ProcessPoolExecutor`

This is the highest-throughput mode. The fused stage chain is applied to each batch in a separate worker process, bypassing the GIL entirely.

**Architecture:**

```
┌──────────────────────┐
│  Reader thread        │  reads source → raw batches
│  (Layer C prefetch)   │
└──────────┬───────────┘
           │  list[dict]  (raw batch)
           ▼
┌──────────────────────┐
│  Main thread          │  submits batches to executor
│  ProcessPoolExecutor  │  collects futures in order
└──────────┬───────────┘
           │  Future[list[dict]]
     ┌─────┴──────┐
     ▼            ▼
┌─────────┐  ┌─────────┐   worker processes
│ Worker 0│  │ Worker 1│   each applies the fused stage chain
│ (copy)  │  │ (copy)  │   to its batch independently
└─────────┘  └─────────┘
           │
           ▼
  list[dict]  (processed batch, in-order)
```

**Critical constraint — picklability**

`ProcessPoolExecutor` serializes work via `pickle`. Everything sent to a worker must be picklable:

- Built-in stages (`RenameFields`, `CastTypes`, `FilterRows`, `DropFields`) are picklable by design — they hold only plain dicts, lists, and callables defined at module level.
- **Lambdas are not picklable.** A `FilterRows(lambda r: r["amount"] > 0)` will fail at the `ProcessPoolExecutor` boundary.
- Custom stages defined as module-level functions are picklable. Class instances with picklable `__dict__` are picklable.

`Pipeline` validates picklability of all stages when `.parallel()` is called, raising `crxml.UnpicklableStageError` early with the name of the offending stage and a suggestion to replace the lambda with a named function.

**Worker function**

The worker receives a raw batch (a `list[dict]`) and a list of `Fusable` stage instances, applies the fused loop, and returns the processed batch:

```python
# parallel.py

def _worker_apply(
    batch: list[dict],
    stages: list[Fusable],
) -> list[dict]:
    """Runs in a worker process. No I/O, no shared state."""
    result = []
    for raw in batch:
        record = raw
        for stage in stages:
            record = stage.apply(record)
            if record is None:
                break
        else:
            result.append(record)
    return result
```

**Executor loop in `Pipeline.__iter__` (parallel mode):**

```python
from concurrent.futures import ProcessPoolExecutor, as_completed
from itertools import islice

def _parallel_iter(
    source: Iterable[dict],
    stages: list[Fusable],
    workers: int,
    batch_size: int,
) -> Iterator[dict]:
    # Layer C prefetch feeds raw batches
    raw_stream = _prefetch_iter(source, batch_size)

    with ProcessPoolExecutor(max_workers=workers) as executor:
        # Keep a sliding window of in-flight futures to preserve order
        futures = []

        def _submit_next():
            batch = list(islice(raw_stream, batch_size))
            if batch:
                futures.append(executor.submit(_worker_apply, batch, stages))
                return True
            return False

        # Pre-fill the window: submit up to 2×workers batches immediately
        # so workers are never idle waiting for the main thread
        for _ in range(workers * 2):
            if not _submit_next():
                break

        while futures:
            fut = futures.pop(0)          # consume in submission order → ordered output
            processed_batch = fut.result()
            yield from processed_batch
            _submit_next()                # refill: submit one new batch per completed one
```

Maintaining submission order via `futures.pop(0)` ensures the output is deterministic and matches the input record order.

**Window size** of `workers * 2` keeps all workers busy: by the time the main thread collects the first result, the next `workers` batches are already running. This eliminates the "submit → wait → submit" stall.

**When to use layer D**

Layer D has overhead: process spawn cost, pickle serialization of batches, and IPC. It is not beneficial for small files. A rough heuristic:

| File size | Recommended mode |
|---|---|
| < 10 MB | Default (layers A–C) |
| 10 MB – 200 MB | `prefetch=True`, no workers |
| > 200 MB | `.parallel(workers=os.cpu_count())` |

The library does not auto-select layer D — the user opts in explicitly via `.parallel()`.

---

### Built-in stages

All built-in stages follow the same pattern: the constructor takes configuration, and the instance is callable as a stage. All implement `Fusable`.

#### `RenameFields`

```python
RenameFields(mapping: dict[str, str])
```

Renames keys in each record according to `mapping`. Keys not in `mapping` are passed through unchanged.

```python
RenameFields({"field_0": "invoice_no", "field_1": "amount"})
```

`apply` implementation:

```python
def apply(self, record: dict) -> dict:
    return {self._mapping.get(k, k): v for k, v in record.items()}
```

#### `CastTypes`

```python
CastTypes(mapping: dict[str, Callable])
```

Applies a type-conversion callable to the specified fields. Conversion errors raise `ValueError` with the field name and original value included in the message.

```python
CastTypes({"amount": float, "issue_date": pd.Timestamp})
```

`apply` implementation:

```python
def apply(self, record: dict) -> dict:
    out = record.copy()
    for field, cast_fn in self._mapping.items():
        if field in out:
            try:
                out[field] = cast_fn(out[field])
            except (ValueError, TypeError) as e:
                raise ValueError(
                    f"CastTypes: cannot cast field '{field}' "
                    f"value {out[field]!r} — {e}"
                ) from e
    return out
```

#### `FilterRows`

```python
FilterRows(predicate: Callable[[dict], bool])
```

Drops records for which `predicate` returns `False`. Returns `None` from `apply` to signal the fused loop to skip the record.

```python
# named function — picklable, works in parallel mode
def positive_amount(r):
    return r["amount"] > 0

FilterRows(positive_amount)
```

`apply` implementation:

```python
def apply(self, record: dict) -> dict | None:
    return record if self._predicate(record) else None
```

#### `DropFields`

```python
DropFields(fields: list[str])
```

Removes the specified keys from each record.

```python
DropFields(["internal_id", "legacy_code"])
```

`apply` implementation:

```python
def apply(self, record: dict) -> dict:
    return {k: v for k, v in record.items() if k not in self._fields_set}
```

`_fields_set` is a `frozenset` built once at construction time.

#### Custom stages

Any callable matching `(Iterable[dict]) -> Iterable[dict]` is a valid stage for single-process mode:

```python
# generator style
def add_vat(stream):
    for record in stream:
        record["amount_with_vat"] = float(record["amount"]) * 1.22
        yield record

# map style — faster: map() is a C builtin
def uppercase_names(stream):
    return map(lambda r: {**r, "client_name": r["client_name"].upper()}, stream)

pipeline = src | add_vat | uppercase_names
```

For parallel mode, implement `Fusable` directly:

```python
class AddVAT:
    """Fusable custom stage — works in parallel mode."""
    def __call__(self, stream: Iterable[dict]) -> Iterable[dict]:
        return map(self.apply, stream)

    def apply(self, record: dict) -> dict:
        return {**record, "amount_with_vat": float(record["amount"]) * 1.22}

pipeline = (src | RenameFields({...}) | AddVAT()).parallel(workers=4)
```

---

### Sinks

#### `to_dataframe`

```python
def to_dataframe(
    pipeline: Iterable[dict],
    chunksize: int | None = None,
) -> pd.DataFrame:
```

Two modes:

- **`chunksize=None` (default)**: collect-then-build. Iterates the full pipeline into a list, then calls `pd.DataFrame.from_records()`. Simple and fast for small-to-medium reports.
- **`chunksize=N`**: chunked mode. Accumulates records in batches of `N`, builds a partial DataFrame per batch, and concatenates with `pd.concat()`. Keeps peak memory bounded for large reports. A `chunksize` of 10 000–50 000 is a reasonable default for most CR exports.

```python
# small report
df = to_dataframe(pipeline)

# large report, bounded memory
df = to_dataframe(pipeline, chunksize=20_000)
```

Implementation:

```python
def to_dataframe(pipeline, chunksize=None):
    if chunksize is None:
        return pd.DataFrame.from_records(iter(pipeline))

    chunks = []
    batch = []
    for record in pipeline:
        batch.append(record)
        if len(batch) >= chunksize:
            chunks.append(pd.DataFrame.from_records(batch))
            batch.clear()
    if batch:
        chunks.append(pd.DataFrame.from_records(batch))
    return pd.concat(chunks, ignore_index=True) if chunks else pd.DataFrame()
```

#### `to_csv`

```python
def to_csv(
    pipeline: Iterable[dict],
    path: str | PathLike,
    *,
    encoding: str = "utf-8",
    delimiter: str = ",",
) -> None:
```

Writes records to a CSV file using `csv.DictWriter`. Streams record-by-record — constant memory regardless of report size. The header is inferred from the keys of the first record.

```python
to_csv(pipeline, "output.csv")
```

#### `collect`

```python
def collect(pipeline: Iterable[dict]) -> list[dict]:
```

Materializes the pipeline into a plain `list[dict]`. Intended for testing, debugging, and small datasets.

---

## Full usage examples

### Minimal

```python
from crxml import CrystalXMLSource, to_dataframe

df = to_dataframe(CrystalXMLSource("report.xml"))
```

### Standard pipeline (layers A–C, default)

```python
from crxml import CrystalXMLSource, RenameFields, CastTypes, FilterRows, DropFields
from crxml import to_dataframe

src = CrystalXMLSource("invoices_export.xml")
print(src.schema())

pipeline = (
    src
    | RenameFields({"field_0": "invoice_no", "field_1": "client", "field_2": "amount"})
    | CastTypes({"amount": float})
    | FilterRows(lambda r: r["amount"] > 0)   # lambda OK in single-process mode
    | DropFields(["internal_id"])
)

df = to_dataframe(pipeline, chunksize=20_000)
```

### Parallel pipeline (layer D)

```python
import os
from crxml import CrystalXMLSource, RenameFields, CastTypes, FilterRows, DropFields
from crxml import to_dataframe

# Named function instead of lambda — required for picklability
def positive_amount(r):
    return r["amount"] > 0

pipeline = (
    CrystalXMLSource("large_report.xml")
    | RenameFields({"field_0": "invoice_no", "field_1": "client", "field_2": "amount"})
    | CastTypes({"amount": float})
    | FilterRows(positive_amount)
    | DropFields(["internal_id"])
).parallel(workers=os.cpu_count(), batch_size=2_000)

df = to_dataframe(pipeline, chunksize=50_000)
```

### Custom Fusable stage for parallel mode

```python
import hashlib
from crxml import CrystalXMLSource, RenameFields
from crxml import to_csv
from typing import Iterable

class AnonymizeClient:
    def __call__(self, stream: Iterable[dict]) -> Iterable[dict]:
        return map(self.apply, stream)

    def apply(self, record: dict) -> dict:
        hashed = hashlib.sha256(record["client"].encode()).hexdigest()[:12]
        return {**record, "client": hashed}


pipeline = (
    CrystalXMLSource("report.xml")
    | RenameFields({"field_0": "invoice_no", "field_1": "client"})
    | AnonymizeClient()
).parallel(workers=4)

to_csv(pipeline, "anonymized.csv")
```

### Schema inspection

```python
src = CrystalXMLSource("unknown_report.xml")
fields = src.schema()
# ['rpt_field_0', 'rpt_field_1', 'rpt_field_2', 'rpt_date', 'rpt_total']

pipeline = src | RenameFields(dict(zip(fields, ["id", "name", "region", "date", "total"])))
```

### Pipeline branching

```python
base = (
    src
    | RenameFields({...})
    | CastTypes({"amount": float})
)

branch_uy = base | FilterRows(lambda r: r["region"] == "UY")
branch_ar = base | FilterRows(lambda r: r["region"] == "AR")

df_uy = to_dataframe(branch_uy)   # re-reads source independently
df_ar = to_dataframe(branch_ar)
```

---

## Execution model summary

```
CrystalXMLSource
      │
      │  raw dict stream (xmlstreamer, C layer, releases GIL on I/O)
      ▼
┌─────────────────────────────────────────────────────────────────────┐
│  Layer C — prefetch thread                                           │
│  threading.Thread reads source → queue.Queue[list[dict]]            │
│  overlaps I/O with downstream processing                            │
└───────────────────────────┬─────────────────────────────────────────┘
                            │  list[dict]  (batches)
                            ▼
┌─────────────────────────────────────────────────────────────────────┐
│  Layer D — ProcessPoolExecutor  (opt-in via .parallel())            │
│  submits batches to worker processes                                 │
│  each worker runs the fused stage loop (Layer B) independently      │
│  results collected in-order via submission-ordered future queue      │
└───────────────────────────┬─────────────────────────────────────────┘
                            │  list[dict]  (processed, ordered)
                            ▼
┌─────────────────────────────────────────────────────────────────────┐
│  Layer B — fused stage loop (single-process) / worker loop (parallel)│
│  single Python for-loop applies all Fusable stages per record        │
│  non-Fusable custom stages fall back to chained generators           │
└───────────────────────────┬─────────────────────────────────────────┘
                            │  dict  (one record at a time)
                            ▼
                          Sink
              (to_dataframe / to_csv / collect)
```

---

## Error handling

- `CrystalXMLSource` raises `FileNotFoundError` if the path does not exist.
- `CrystalXMLSource` raises `crxml.InvalidCRXMLError` (subclass of `ValueError`) if the file is not a recognized CR XML export.
- `CastTypes` raises `ValueError` on conversion failure, including field name and raw value in the message.
- `Pipeline.parallel()` raises `crxml.UnpicklableStageError` (subclass of `TypeError`) if any stage cannot be pickled, with the stage name and a suggestion to replace lambdas with named functions.
- Stage exceptions propagate naturally through the iterator chain — no silent swallowing.
- Worker process exceptions are re-raised in the main process by `Future.result()`, preserving the original traceback.

---

## Dependencies

| Package | Role |
|---|---|
| `xmlstreamer` | Streaming XML parser (carlosplanchon) |
| `pandas` | DataFrame construction |

Python >= 3.10 required. No external dependencies beyond the two above; all concurrency primitives (`threading`, `queue`, `concurrent.futures`) are stdlib.

---

## Out of scope (future)

- Polars sink (`to_polars()`).
- Async source for reading from remote URIs.
- Schema inference with dtype auto-detection.
- Support for CR XML subreports (nested `<Subreport>` elements).
- CLI entrypoint (`crxml report.xml --output out.csv`).
- Auto-selection of layer D based on file size heuristic.
