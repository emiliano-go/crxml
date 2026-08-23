# Architecture

## Overview

crxml is a fast XML-to-DataFrame pipeline that uses a Rust core for parsing and a Python layer for pipeline composition. The key architectural insight is **fusion**: each stage of a pipeline can be compiled down into the Rust rypipe engine, executed as a vectorized batch operation on Arrow arrays, or fused into a tight dict loop, depending on what the stages support.

### Fusion levels

The three fusion levels are tried in priority order:

1. **Columnar fusion** (Layer A): compile stages into the Rust `ExecutionPlan`, eliminating row iteration entirely. Stages with `_plan_kwargs()` produce a merged kwargs dict passed to `read_to_columnar*`. Remaining stages run through the batchpipe chain over Arrow `RecordBatch` objects.
2. **Vectorized batch fusion**: arrow-fusable stages (rename, drop, declarative filter) compile to `Callable[[Batch], Batch]` functions clustered into a single-pass `FusedTransforms` operator. Row-local `.apply` stages cluster into `LambdaOp`. Trailing stateful generators wrap the dict stream.
3. **Dict fusion** (Layer B): a contiguous run of `.apply` stages is fused into one tight `for r in src: for fn in bound: ...` loop.

These stack: columnar pushdown is tried first; if it succeeds, the remaining stages run through the batchpipe; any trailing stateful stages wrap the dict stream. If columnar fusion isn't possible (source lacks `_read_arrow`, or no stage produces `_plan_kwargs`), the system falls back to dict fusion only.

```
XML bytes ──► rypipe engine (via crxml wrapper) ──► Arrow Table ──► batchpipe chain ──► sink

The engine has three modes:
  stream:    row-by-row via CrxmlReader (GIL-released batching)
  columnar:  single-threaded columnar parse with ExecutionPlan pushdown
  parallel:  chunked + rayon parallel columnar parse
```

## Data flow

```
XML file
  │
  ├─► stream engine: CrxmlReader.next_batch(n)
  │     │
  │     └─► list[dict[str,str]] ──► Pipeline stages ──► sink
  │
  ├─► columnar engine: rypipe_core::TableBuilder via crxml wrapper
  │     │
  │     ├─► simdutf8 validation (one SIMD pass) in rypipe_xml::CrystalXmlDecoder
  │     ├─► borrowed-slice quick-xml reader (zero-copy events)
  │     │
  │     ├─► ColumnBuilder columns ──► finish_row (null-fill, filter)
  │     │     └─► TableBuilder::extend (merge across chunks for multi/parallel)
  │     │
  │     └─► RecordBatch export / engines_to_record_batches
  │           │
  │           └─► Arrow C Data Interface ──► pyarrow.Table
  │                 │
  │                 ├─► batchpipe chain (build_chain)
  │                 │     ├─► ArrowSource ──► FusedTransforms ──► LambdaOp
  │                 │     ├─► iter_dicts() or collect_table()
  │                 │     └─► trailing stateful stages wrap stream
  │                 │
  │                 ├─► Compare filter: arrow::compute kernels (pure Rust)
  │                 │
  │                 └─► sinks: to_dataframe / to_csv / collect / to_polars / to_parquet
  │
  └─► parallel engine: rypipe_xml::CrystalXmlSplitter + rypipe_core::ParallelExecutor
        │
        ├─► fast path (no auto_dict): per-chunk TableBuilders exported independently
        │     └─► engines_to_record_batches() → per-chunk RecordBatch → concat
        │
        └─► merge path (auto_dict): TableBuilders merged → auto_dict_upgrade → export
              └─► TableBuilder::extend() → RecordBatch
```

## Rust core (`crxml_core`)

The crate at `src/crxml_core/` uses `mimalloc::MiMalloc` as the global allocator (profiling showed ~27% of CPU time in malloc/free during XML parsing). It now contains two layers:

1. **Streaming engine** (`CrxmlReader` / `RowParser`): Crystal Reports XML specific and stays in `crxml_core`.
2. **Columnar FFI wrappers**: thin Python-callable wrappers that delegate to the generic `rypipe` engine in the sibling workspace (`../rypipe/`).

The format-agnostic pieces (`ExecutionPlan`, `ColumnBuilder`, parallel/ bounded drivers, Arrow export, and the Crystal XML decoder/splitter) live in the `rypipe` workspace:
- `rypipe-core`: generic engine
- `rypipe-xml`: Crystal Reports XML adapter
- `rypipe-python`: standalone PyO3 bindings (used here only for helper fns where convenient)

### `lib.rs`: FFI boundary, stream engine, and columnar wrappers

#### Global allocator

```rust
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

#### `CrxmlReader` (`#[pyclass]`): streaming XML parser

**`RowParser`** is a pure-Rust struct that holds **no Python objects**. This is a load-bearing invariant: it allows `next_batch(n)` to release the GIL via `py.allow_threads()` (the `Ungil` bound on the closure requires no `Py<...>` references inside). Python-object state (the interned-key cache) lives on `CrxmlReader` beside this struct.

```rust
struct RowParser {
    reader: Reader<BufReader<File>>,   // quick-xml streaming reader (128 KB buffer)
    buf: Vec<u8>,                      // scratch for quick-xml events
    inner_buf: Vec<u8>,                // scratch for child-element events
    row: Vec<(String, String)>,        // per-row field-value pairs (cleared each row)
    row_tag: Vec<u8>,                  // e.g. b"Row"
    batch_vals: Vec<(String, String)>, // flat buffer: all rows concatenated
    batch_lens: Vec<usize>,            // field count per row for slicing
}
```

**Parse flow** (`read_one_row`):
1. Quick-xml event loop looking for `<Row>` (or custom `row_tag`).
2. For `<Row Start>`, enters a child event loop looking for `<Field>`, `<Text>`, `<Section>`.
3. `<Field>`: extracts `FieldName` attribute → key, reads `<FormattedValue>` or `<Value>` text → value.
4. `<Text>`: extracts `Name` attribute → key, reads `<TextValue>` text → value.
5. `<Section>`: captures `SectionNumber` attribute.
6. Collects into `row: Vec<(String, String)>`.

**`read_batch_into(n)`**: calls `read_one_row` up to `n` times, extending `batch_vals` and `batch_lens`. Runs with the GIL released.

**Dict construction** (GIL held, `#[pymethods]`):
- `new_dict(py)`: creates a plain `PyDict`. The private CPython API `_PyDict_NewPresized` was benchmarked and removed; it delivered only 3.5% overall gain and used an `unsafe` call to a private, unstable symbol.
- `cached_key(key)`: `FxHashMap<String, Py<PyString>>`: field names repeat every row; interned `PyString` objects are reused instead of allocating fresh `PyUnicode` per field per row.

**`next_batch(n)`**: releases GIL → parses `n` rows into flat buffers → re-acquires GIL → walks `batch_vals`+`batch_lens` → builds `PyList[PyDict]`.

#### Columnar FFI functions

Four `#[pyfunction]` entry points are thin wrappers over `rypipe-core` / `rypipe-xml`:

| Function | Parsing | Output |
|----------|---------|--------|
| `read_to_columnar` | Single-threaded via `rypipe_xml::CrystalXmlDecoder` + `rypipe_core::TableBuilder` | One `pyarrow.Table` |
| `read_to_columnar_multi` | Chunked via `rypipe_xml::CrystalXmlSplitter`, sequential parse + `TableBuilder::extend` | One `pyarrow.Table` |
| `read_to_columnar_par` | Chunked + `rayon` via `rypipe_core::ParallelExecutor` | Per-chunk batch concat (fast path) or merged table (auto_dict) |
| `read_to_columnar_bounded` | Memory-bounded batches via `rypipe_core::BoundedExecutor` | Concatenated `pyarrow.Table` |

All accept the same `ExecutionPlan` kwargs as before: `field_mapping`, `drop_fields`, `filter`, `field_types`, `dictionary_columns`, `use_mmap`, `schema`, `auto_dict`.

The wrappers:
1. Build an `rypipe_core::ExecutionPlan` from Python kwargs (same semantics as the old `BuildPlan`).
2. Open the input via `rypipe_core::InputBuffer` (mmap when requested).
3. Drive `CrystalXmlDecoder` / `CrystalXmlSplitter` / `ParallelExecutor` / `BoundedExecutor`.
4. Finish the `TableBuilder` to an Arrow `RecordBatch`, apply any column-to-column `Compare` filter with `rypipe_core::arrow_export::apply_compare_filter`, and export via the Arrow C Data Interface.

Compare filters are now evaluated in pure Rust with `arrow::compute` kernels; the previous `pyarrow.compute` call from inside Rust has been removed.

### Where the columnar engine lives now

The files `src/crxml_core/src/columnar.rs` and `src/crxml_core/src/splitter.rs` have been removed. Their contents were extracted into the sibling `rypipe` workspace:

- `rypipe-core::plan::ExecutionPlan`: the format-agnostic plan (renamed from `BuildPlan`).
- `rypipe-core::columnar`: `StrColumn`, `ColumnBuilder`, and dictionary encoding.
- `rypipe-core::engine::TableBuilder`: the per-chunk state machine (renamed from `ColumnarEngine`).
- `rypipe-core::merge`: engine merging and fast parallel export.
- `rypipe-core::arrow_export`: Arrow `RecordBatch` building and `Compare` filter evaluation via `arrow::compute` kernels.
- `rypipe-core::decoder`: the `Splitter`, `RecordParser`, and `ColumnarSink` traits.
- `rypipe-core::parallel` / `rypipe-core::bounded`: parallel and memory-bounded drivers.
- `rypipe-core::input`: `InputBuffer` with optional mmap support.
- `rypipe-xml::decoder::CrystalXmlDecoder`: Crystal Reports XML row parser.
- `rypipe-xml::splitter::CrystalXmlSplitter`: XML row-boundary splitting.

crxml still owns the Crystal-specific grammar, but as a `rypipe` format adapter rather than inline engine code. Future formats (CSV, NDJSON, generic XML, HTML) can be added as additional `rypipe` adapters without touching crxml.

## Python source layer

### `__init__.py`: Lazy public API

```python
__all__ = ["CrystalXMLSource", "Pipeline", "RenameFields", "CastTypes",
           "FilterRows", "DropFields", "to_dataframe", "to_csv", "collect"]

_modules = {"CrystalXMLSource": ".source", "Pipeline": ".pipeline", ...}

def __getattr__(name):
    if name in _modules:
        mod = importlib.import_module(_modules[name], __package__)
        return getattr(mod, name)
    raise AttributeError(...)
```

All public symbols are lazily imported via `__getattr__`. `import crxml` is instant; modules load only when their symbols are first accessed.

### `CrystalXMLSource` (in `source.py`)

Wraps the Rust engines. Constructor parameters map one-to-one to `ExecutionPlan` fields plus engine selection:

- `engine`: `"auto"` (default), `"stream"`, `"columnar"`, `"parallel"`
- `threads`: multiplied by **4** to get `num_chunks`. The 4x multiplier exists because finer chunks give better load balancing; VTune showed 3-4x optimal on 24 cores (beyond 4x, rayon join/spin overhead dominates).
- `memory`: optional string (`"8GB"`) or int bytes; enables bounded mode.
- `use_mmap`: memory-map the file (Unix only).
- `batch_size`: rows per Rust→Python batch call (default 1024).
- `field_mapping`, `drop_fields`, `filter`, `field_types`, `dictionary_columns`, `schema`, `auto_dict`: map directly to `ExecutionPlan`.

**Goal-aware engine dispatch** (`_resolve_engine(goal)`):

| Goal | File size | Memory OK | Engine selected |
|------|-----------|-----------|-----------------|
| `"iter"` | any | any | `"stream"` (always) |
| `"table"` | ≥ 8 MB | yes | `"parallel"` |
| `"table"` | ≥ 8 MB | no | `"columnar"` (if available) → `"stream"` fallback |
| `"table"` | < 8 MB | yes | `"columnar"` (if available) → `"stream"` fallback |

The 8 MB threshold exists because parallel overhead (chunking + rayon + merge) doesn't pay off for small files.

**`_build_plan_kwargs()`**: collects the source's config into a dict (`field_mapping`, `drop_fields`, `filter`, `field_types`, `dictionary_columns`, `schema`, `auto_dict`, `use_mmap`). This is the base dict that stage `plan_overrides` are merged into.

**`_read_arrow(plan_overrides)`**: the core table-building method.
1. Resolves engine for `"table"` goal.
2. Merges `plan_overrides` into `_build_plan_kwargs()`.
3. Dispatches to Rust function: bounded → `read_to_columnar_bounded`; columnar → `read_to_columnar`; parallel → `read_to_columnar_par`; stream fallback → builds `pyarrow.Table` from Python dicts.
4. Caches the result (unless `plan_overrides` was provided, indicating a one-off fusion call).

**Iteration modes**:

- `__iter__`: stream → `_batch_iter(self._stream_iter())` calls `CrxmlReader.next_batch` in a loop, yielding dicts. Columnar/parallel → `_arrow_iter(self._read_arrow())` walks `Table.to_batches()`, yielding via `batch.to_pylist()`.
- `_iter_batches`: stream → calls `reader.next_batch` directly (yields lists of dicts). Columnar/parallel → calls `to_arrow().to_batches()` then `.to_pylist()` per batch.

**`_batch_iter(reader, batch_size)`**: wraps `CrxmlReader.next_batch` in a generator. One Rust call per batch with GIL released; `yield from batch` walks each batch list at C speed (no per-row Python `__next__`).

**`_arrow_iter(table)`**: walks `table.to_batches()` and yields dicts via `batch.to_pylist()`.

**`schema()`**: reads the first row via `next(iter(self), None)`, returns `list(first_row.keys())` or `[]` for empty files. The first batch is cached internally (via `_cached_arrow` for columnar, or the stream reader's internal state for stream) so schema inspection doesn't consume data.

### `Pipeline` (in `pipeline.py`)

`Pipeline` is an immutable value object.

```python
class Pipeline:
    __slots__ = ("_source", "_stages", "_batch_size", "_prefetch", "_workers")
```

**`__or__(stage)`**: creates a new `Pipeline` with the stage appended. The original is unchanged:
```python
source | rename | cast  →  Pipeline(source, [rename, cast])
```

**`__iter__()`**: decides execution strategy:
1. If `self._workers` is set → `parallel.parallel_iter()` (ProcessPoolExecutor). Stages are validated as picklable first.
2. Otherwise → `fusion.fused_iter(source, stages)`.

**`_to_arrow()` shortcut**:
- Returns a single `pyarrow.Table` if the whole pipeline can be executed as columnar fusion + batchpipe chain without trailing stages.
- Returns `None` if: workers are set, source lacks `_read_arrow`, or trailing stateful stages remain.
- This is the key fast path used by `to_dataframe()` and `collect()`; they check `_to_arrow()` first and skip the dict stream entirely.

**`parallel(workers=None, batch_size=1000)`**: returns a new `Pipeline` with worker count set. Also carries forward the `_prefetch` flag (though prefetch is not toggled by the current public API).

### `fusion.py`: Fusion orchestrator

**`plan_split(stages)`**: iterates stages calling `_plan_kwargs()` on each. Stages returning a dict contribute to `plan_overrides` (consumed); stages returning `None` or lacking the method go to `remaining`.

```
Input:  [RenameFields, CastTypes, FilterRows(callable), DropFields]
Output: plan_overrides = {field_mapping: ..., field_types: ..., drop_fields: ...}
        remaining = [FilterRows(callable)]
```

**`_try_columnar_fusion(source, stages)`**:
1. Guards: source must have `_read_arrow` and `_build_plan_kwargs`.
2. Calls `plan_split` → gets `plan_overrides` and `remaining`.
3. If no plan_overrides and all stages are remaining → returns `None` (no columnar benefit).
4. Calls `source._read_arrow(plan_overrides=plan_overrides or None)` → produces `pyarrow.Table` with Rust pushdown.
5. Calls `batchpipe.build_chain(table, remaining, batch_size)` → returns Volcano operator chain + trailing stages.
6. Returns `iter_dicts(op)` stream; trailing stages wrap the stream.

**`fused_iter(source, stages)`**: the main execution entry point:
1. Tries `_try_columnar_fusion`: if it returns a non-None stream, done.
2. Falls back to dict fusion:
   - Scans from front for a contiguous run of fusable stages (has `callable(stage.apply)`).
   - Fused inner loop: `for r in src: for fn in bound: r = fn(r); if None: break; else: yield r`.
   - Non-fusable remaining stages wrap the fused generator.
   - If no fusable stages found: source bypasses the fused generator (avoids one generator frame per row). Stages wrap the source directly.

**`is_fusable(stage)`**: `callable(stage.apply)`.

### `batchpipe.py`: Vectorized batch pipeline

A pull-based (Volcano-style) operator chain over Arrow `RecordBatch` objects.

**`Batch`**: the unit of flow. `namedtuple("Batch", "data, selection")` where `data` is a `RecordBatch` and `selection` is an optional `BooleanArray` mask.

```python
class Batch:
    def compact(self):
        """Apply the selection and return a dense RecordBatch."""
        if self.selection is None:
            return self.data
        return self.data.filter(self.selection)
```

**Operator hierarchy**:

```
Operator (abstract)
  open(), next_batch() -> Batch | None, close()

├── ArrowSource(table, batch_size)
│     └── wraps pyarrow.Table, yields Batch objects

├── FusedTransforms(upstream, fns)
│     └── applies list of batch-level functions (rename/drop/filter)

└── LambdaOp(upstream, applies)
      └── row-level .apply fallback: compact → dict → apply → rebuild RecordBatch
```

**Selection masks**: filters produce boolean masks via `pyarrow.compute` (e.g., `pc.equal(rb.column("city"), "NYC")`). Masks are AND-ed into `Batch.selection`. Compaction (`Batch.compact()` → `RecordBatch.filter(selection)`) happens only at sinks or at `LambdaOp` boundaries, avoiding materialization of filtered-out rows until necessary.

**Arrow-fusable stages** compiled by `_arrow_fusable(stage)`:

| Stage | Compiles to | Implementation |
|-------|------------|----------------|
| `RenameFields` | `_fuse_rename(mapping)` | `RecordBatch.from_arrays(rb.columns, names=[mapping.get(n,n) for n in names])` |
| `DropFields` | `_fuse_drop(fields)` | Keep columns by index, rebuild batch |
| `FilterRows` (declarative) | `_fuse_filter_spec(spec)` | `pc.equal(column, value)` → AND into selection. Compare: `pc.greater(cola, colb)` etc. Null fill matches dict semantics |

**Not arrow-fusable**: `CastTypes` (type coercion in Arrow is not a simple rename/drop/filter), `FilterRows` with callable predicate, lambda stages, generators.

**`build_chain(table, stages, batch_size)`**:
1. Starts with `ArrowSource(table, batch_size)`.
2. Greedily clusters arrow-fusable stages into a single `FusedTransforms`.
3. Clusters consecutive row-level `.apply` stages into a single `LambdaOp` (only at boundaries where `_arrow_fusable` returns None and stage has `.apply`).
4. Stops at generic stream stages (no `.apply`, no arrow fusion).
5. Returns `(operator, trailing_stages)`.

**Sinks**:
- `iter_dicts(op)`: compact each batch, yield via `RecordBatch.to_pylist()`.
- `collect_table(op)`: collect all compacted batches, return `pa.Table.from_batches(batches)`.

### `stages/`: The four built-in stages

All four implement the same protocol:

```python
class Stage:
    def apply(self, record: dict) -> dict | None: ...
    def __call__(self, stream): return map(self.apply, stream)
    def _plan_kwargs(self) -> dict | None: ...
```

| Stage | `apply()` behavior | `_plan_kwargs()` output |
|-------|-------------------|------------------------|
| `RenameFields(mapping)` | `{mapping.get(k,k): v for k,v in record.items()}` | `{"field_mapping": mapping}` |
| `CastTypes(mapping)` | `record[field] = cast_fn(record[field])` in-place | `{"field_types": {name: type_str}}`. Maps `int→"int64"`, `float→"float64"`, `bool→"bool"`, `str→None` (skip). Returns `None` if any cast fn is not one of these |
| `DropFields(fields)` | `{k:v for k,v in record.items() if k not in fields_set}` | `{"drop_fields": sorted(fields_set)}` |
| `FilterRows(...)` | `record if predicate(record) else None` | `{"filter": spec}` for declarative; `None` for callable |

**`FilterRows`** has three construction paths:
1. **Callable predicate**: `FilterRows(predicate=lambda r: ...)`: not columnar-pushdownable.
2. **Declarative constant**: `FilterRows(field="city", op="==", value="NYC")`: pushdownable as `FilterPredicate::Equal`/`NotEqual`. Uses `_ConstantPredicate` inner class.
3. **Declarative compare**: `FilterRows(field_a="age", op=">", field_b="threshold")`: pushdownable as `FilterPredicate::Compare` (post-reduce via pyarrow.compute). Uses `_ComparePredicate` inner class.

**Filter semantics**:
- Constant `==`: missing field returns unequal (dict `.get()` returns `None`).
- Constant `!=`: missing field returns equal (`None != value` is true; the row is kept).
- Compare: both columns must exist. Evaluated post-reduce via `pyarrow.compute`.

### `parallel.py`: Multi-process parallelism

```
Source ──► _prefetch_iter (bounded queue, maxsize=8) ──► ProcessPoolExecutor ──► ordered results
```

**`_prefetch_iter(source, batch_size, maxsize=8)`**: background `threading.Thread` reads the source, fills a `queue.Queue` with dict batches (size `batch_size`). Bounded at 8 batches to prevent unbounded memory.

**`validate_stages_picklable(stages)`**: pickles each stage and raises `TypeError` at `.parallel()` call time (not in the worker). Catches lambdas and closures early.

**`_worker_apply(batch, stages)`**: module-level function (required for pickling). Re-imports `fused_iter` from `.fusion` inside the worker process, runs `list(fused_iter(batch, stages))` and returns the result list.

**`parallel_iter(source, stages, workers, batch_size)`**:
1. Wraps source in `_prefetch_iter`.
2. Creates `ProcessPoolExecutor(max_workers=workers)`.
3. **Double-buffered submission**: submits `workers * 2` futures initially. For each completed future, submits one new future. This keeps the executor saturated while bounding in-flight memory.
4. Yields results in submission order: `for idx in range(len(futures)): yield from futures[idx].result()`.

### `sinks.py`: Terminal operations

**Shortcut hierarchy**:

| Sink | Fast path | Fallback |
|------|-----------|----------|
| `to_dataframe` | `pipeline._to_arrow()` → single `pyarrow.Table` → `table.to_pandas()` (ArrowDtype, zero dicts) | `_iter_batches()` → chunked DataFrame → `pd.concat`; or `pd.DataFrame.from_records(iter(pipeline))` |
| `collect` | `pipeline._to_arrow()` → `table.to_pylist()` | `_iter_batches()` → dicts; or `list(pipeline)` |
| `to_csv` | Always streams row-by-row via `csv.DictWriter` (no intermediate list) | - |

`to_dataframe(chunksize=N)` always uses batch-then-concat for memory control. `chunksize=None` triggers the single-table fast path.

## Fusion decision tree

When `Pipeline.__iter__()` is called (not in worker mode):

```
fused_iter(source, stages)
  │
  ├─ Has _read_arrow + _build_plan_kwargs?
  │     ├─ NO  ──► skip to dict fusion
  │     └─ YES ──► plan_split(stages)
  │                   │
  │                   ├─ plan_overrides empty AND len(remaining) == len(stages)?
  │                   │     └─ YES ──► skip to dict fusion (no columnar benefit)
  │                   │
  │                   └─ NO ──► Layer A: source._read_arrow(plan_overrides)
  │                               │
  │                               └─ build_chain(table, remaining, batch_size)
  │                                     │
  │                                     ├─ Returns (op, trailing)
  │                                     │     op = ArrowSource → FusedTransforms → LambdaOp
  │                                     │     trailing = [stateful stream stages]
  │                                     │
  │                                     └─ stream = iter_dicts(op)
  │                                         for stage in trailing: stream = stage(stream)
  │                                         return stream
  │
  └─ Dict fusion (Layer B):
       │
       ├─ Scan front: contiguous .apply stages → fusables
       ├─ bound = [s.apply for s in fusables]
       │
       ├─ If no bound:
       │     stream = source (or _iter_batches flat)
       │     for stage in remaining: stream = stage(stream)
       │
       └─ If bound:
             def fused():
               for r in source:
                 for fn in bound:
                   r = fn(r); if None: break
                 else: yield r
             stream = fused()
             for stage in remaining: stream = stage(stream)
             return stream
```

When a sink is called:

```
to_dataframe(pipeline):
  ├─ has _to_arrow()?
  │     └─ YES → pipeline._to_arrow()
  │                  ├─ returns Table? → table.to_pandas()  [FASTEST]
  │                  └─ returns None? → fallback
  ├─ has _iter_batches()?
  │     └─ YES → [pd.DataFrame.from_records(batch) for batch in pipeline._iter_batches()]
  └─ pd.DataFrame.from_records(iter(pipeline))
```

## Memory model

- **Stream engine**: `RowParser` reuses `Vec<u8>` buffers across rows. Dicts are built in Python heap via `PyDict::new()`. The `FxHashMap` key cache lives for the reader's lifetime.

- **Columnar engine**: `StrColumn` uses a flat byte arena + `i32` offsets; no per-cell `String` allocation. Numeric columns use `Vec<Option<i64>>` (8 bytes + 1 validity per cell). Arrow arrays are built natively and exported via C Data Interface.

- **mmap**: files are memory-mapped; advice is `MADV_WILLNEED` when `prefault` is set (parse-speed goal) and `MADV_SEQUENTIAL` otherwise (RSS-sensitive paths). The mapping is dropped synchronously after export; all data lives in owned Arrow buffers by then.

- **Bounded mode**: `read_to_columnar_bounded` samples 64 KB → estimates `bytes_per_row` → splits into memory-sized chunks → parses/exports each chunk independently → chunk engine dropped → tables concatenated.

- **Parallel mode RSS**:
  - Without auto_dict: each chunk's `TableBuilder` is exported and dropped before next is processed → peak RSS ≈ file size + overhead.
  - With auto_dict: all chunk engines held in memory before merge + dict upgrade → peak RSS can reach ~5x file size.

## Concurrency model

| Component | Concurrency mechanism | GIL behavior |
|-----------|----------------------|--------------|
| Stream parser (`CrxmlReader`) | Single-threaded | Released during `read_batch_into` |
| Columnar single (`read_to_columnar`) | Single-threaded | Released during parse, held for export |
| Columnar multi (`read_to_columnar_multi`) | Sequential chunks | Released per chunk parse |
| Columnar parallel (`read_to_columnar_par`) | `rayon::par_iter()` | Released for entire parallel parse (only GIL at start and end) |
| Prefetch reader thread | `threading.Thread` | Held by reader for dict construction |
| Parallel pipeline (`ProcessPoolExecutor`) | Separate processes | No GIL contention (separate interpreters) |
| Batchpipe chain (FusedTransforms/LambdaOp) | Single-threaded (consumer) | Held (Arrow operations with GIL) |

The expensive parts (XML parsing, string scanning) run with the GIL released in all paths. The columnar engine goes further: it never creates Python objects during parsing, so GIL release is more effective (no periodic Python GC interference).

## Key optimization summary

| Optimization | Location | Impact |
|-------------|----------|--------|
| `mimalloc` global allocator | `lib.rs:22` | ~27% CPU reduction in malloc/free |
| `PyDict::new` (no presize) | `src/crxml_core/src/lib.rs` | Removed private-CAPI hack; 3.5% gain not worth `unsafe` |
| Key interning (`FxHashMap`) | `src/crxml_core/src/lib.rs` | Reuses `PyString` objects across rows |
| SIMD UTF-8 validation | `rypipe_xml::decoder` | One SIMD pass per chunk (via `simdutf8`) |
| Fast scanner (memchr-based) | `rypipe_xml::decoder` | Avoids quick-xml event loop overhead for standard CR XML |
| `StrColumn` arena allocation | `rypipe_core::columnar` | No per-cell `String` allocation |
| Deferred filter compaction | `batchpipe.py:31-48` | Only materializes alive rows at sinks/LambdaOp |
| Columnar fusion (Layer A) | `fusion.py:23-44` | Entire pipeline compiled into Rust `ExecutionPlan` |
| `_to_arrow()` shortcut | `pipeline.py:45-67` | Skips dict construction entirely for fast-path pipelines |
| Arrow C Data Interface | `rypipe_core::arrow_export` | Zero-copy export from Rust Arrow to pyarrow |
| Synchronous unmap after export | `rypipe_core::input` (`MmapInput`) | Releases file-backed pages before pandas conversion begins |
| 4x chunk multiplier | `source.py:109` | Finer grains for rayon load balancing (VTune-optimized) |
| Bounded memory batches | `rypipe_core::bounded` | Streams large files within configurable memory budget |
| Fast parallel export (no merge) | `rypipe_core::merge::engines_to_record_batches` | Avoids per-chunk merge for non-auto-dict parallel parse |

## Key data types

| Context | Type | Role |
|---------|------|------|
| Python stream | `dict[str, str]` | Single row (raw string values) |
| Python columnar | `pyarrow.Table` | Full parsed dataset in Arrow format |
| Python batchpipe | `Batch` (class with `__slots__`) | Unit of flow: `data` (`RecordBatch`) + optional boolean `selection` mask |
| Python pipeline | `Pipeline` | Immutable composition of `source + stages` |
| Python stage | `Callable[[Iterable[dict]], Iterable[dict]]` | Row transformation function |
| Rust stream | `CrxmlReader` (PyClass) | Streaming XML parser |
| Rust columnar | `rypipe_core::TableBuilder` | HashMap of `ColumnBuilder`s + plan + row count |
| Rust columnar | `rypipe_core::ColumnBuilder` | String / Int64 / Float64 / Boolean / Dictionary variants |
| Rust columnar | `rypipe_core::StrColumn` | Flat byte arena + `i32` offsets (Arrow layout) |
| Rust columnar | `rypipe_core::ExecutionPlan` | Compilation target for stage pushdown |
| Rust columnar | `rypipe_core::FilterPredicate` | Equal / NotEqual / Compare variants |
| Rust splitter | `rypipe_xml::CrystalXmlSplitter` | Finds whole-row split points for parallel parsing |
| Rust decoder | `rypipe_xml::CrystalXmlDecoder` | Emits field events from Crystal Reports XML |
