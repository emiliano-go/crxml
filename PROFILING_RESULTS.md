# VTune Profiling Results & Optimizations

## Baseline (pre-opt)

### VTune Hotspots (100 MB benchmark, ~0.56s parse time)

| Rank | Function | CPU Time | % | Source |
|------|----------|----------|---|--------|
| 1 | `RtlAllocateHeap` (ntdll) | 0.106s | 9.0% | System allocator |
| 2 | `read_event_impl` (quick-xml) | 0.104s | 8.9% | `quick-xml::Reader` loop |
| 3 | `malloc_base` (ucrtbase) | 0.078s | 6.7% | System allocator |
| 4 | `LoaderLibraryExW` (KERNEL32) | 0.056s | 4.8% | DLL loading (warmup) |
| 5 | `emit_start` (quick-xml) | 0.046s | 4.0% | Start-tag emission |
| 6 | `__rdl_alloc` (Rust alloc) | 0.046s | 4.0% | `alloc::vec` / `String` |
| 7 | `fill_buf` (BufReader) | 0.045s | 3.8% | File I/O buffering |
| 8 | `__rdl_realloc` (Rust alloc) | 0.045s | 3.8% | Vec/String reallocation |
| 9 | **`crxml_core::CrxmlReader::__next__`** | 0.045s | 3.8% | **Our parser loop** |
| 10 | `fill_buf` (BufReader) | 0.045s | 3.8% | File I/O buffering |
| 11 | `__rdl_dealloc` (Rust alloc) | 0.030s | 2.6% | Memory freeing |
| 12 | `memchr` AVX2 (various) | ~0.078s | ~6.7% | SIMD byte search |
| — | `RtlFreeHeap` (ntdll) | 0.016s | 1.4% | System dealloc |
| **Total allocation** | **(alloc + realloc + dealloc + heap)** | **~0.321s** | **~27.4%** | |

### Key Insight

**~27% of CPU spent on allocation/deallocation.** Our `__next__` (3.8%) looks small but triggers most allocations via:

- `Vec<(String, String)>` row buffer (per-row allocation pool)
- `String::new()` for every field's text content (per-field allocation)
- `Option<String>` temporary for field name lookup (per-Field/Text allocation)
- `key.to_owned()` + `value.into_owned()` per row attribute
- Intermediate for-loop copying row `Vec` → `PyDict`

## Changes Made

### Removed `row: Vec<(String, String)>` entirely

**Before:** Attributes and field values collected into a `Vec<(String, String)>`, then copied to `PyDict` at row end. Two allocations per entry (key + value).

**After:** `PyDict::new(py)` created at row start. `dict.set_item(key, value)?` called directly per field. Zero Rust-side String allocations for storage.

### Added reusable `key_buf: String`, `text_buf: String`

**Before:** `let mut field_name: Option<String> = None`, `let key = field_name.unwrap_or_else(...)`, `let mut text = String::new()`. Fresh allocation per Field/Text child.

**After:** `key_buf.clear(); key_buf.push_str(value.as_ref())` — reuses heap capacity across all fields in a row. Same for `text_buf`. Saves ~2-3 allocations per child element.

### Direct `PyDict` insertion

**Before:**
```
row.push((key_name, field_text));
// ... end of row ...
for (k, v) in std::mem::take(row) { dict.set_item(k, v) }
```

**After:**
```
dict.set_item(key_buf.as_str(), text_buf.as_str())?;
```
No intermediate storage. Python string created once per field instead of Rust String + Python String.

## Round 2 (allocator + key cache + I/O)

Changes:

1. **mimalloc as Rust global allocator** -- all Rust-side allocations bypass the Windows system heap (`RtlAllocateHeap` was 9% alone, total alloc ~27%).
2. **Cached Python key strings** -- field names repeat identically every row; `CrxmlReader` now keeps a `Vec<(Vec<u8>, Py<PyString>)>` and reuses the same `PyString` object per key instead of allocating a fresh `PyUnicode` per field per row. Linear scan; rows have 10-30 distinct keys.
3. **BufReader 128 KB -> 512 KB**.

Result (100 MB benchmark): **0.479s vs 0.56s baseline, ~14% faster** (188,645 rows/s, 208.6 MB/s).

## Round 3 (key cache Vec -> FxHashMap)

VTune re-profile after Round 2 (2 runs, 5x100 MB iterations each) showed the Round 2
`cached_key` linear scan had become a top hotspot: `memcmp` 5.5% + scan body 2.4% ~= 8%
of CPU (every field of every row byte-compared against ~15-20 cached keys). Module
rollup: _crxml_core.pyd 73%, VCRUNTIME 14% (memcpy=mi_realloc + memcmp), python313 10%.

Change: `key_cache` is now `FxHashMap<Vec<u8>, Py<PyString>>` (rustc-hash) -- hash once
instead of N memcmps per field.

Result (100 MB benchmark): **0.402s vs 0.479s, ~16% faster** (224,956 rows/s, 248.8 MB/s).
Cumulative vs 0.56s baseline: ~28%.

Remaining candidates from that profile: realloc/memcpy churn from quick-xml buffer
regrowth (8.3%, est 2-4% win via bigger pre-reserve); per-value `PyUnicode_DecodeUTF8`
(4.2%, values unique so not cacheable -- skip).

## Round 4 (buffer clears + quick-xml config)

VTune re-profile (3 runs) after Round 3 confirmed: memcmp hotspot gone (-80%),
key_cache FxHashMap now only 1.8%. New module split: pyd 76%, python313 16%,
VCRUNTIME 7%.

Changes:

1. **`buf.clear()` / `inner_buf.clear()` before every `read_event_into`** --
   quick-xml appends into the buffer; previously `buf` was only cleared on the
   Empty-row path, so it grew (realloc + memcpy) across the entire file.
   Wall-clock roughly neutral (memset/realloc per run dropped ~35%) but fixes
   unbounded buffer growth.
2. **`check_end_names = false`** -- we match end tags ourselves; skips
   quick-xml's open-tag stack bookkeeping. ~3% (within noise).

Result: 100 MB median **~0.41s** across runs (best 0.38s, 261 MB/s). Cumulative
vs 0.56s baseline: ~27-32% depending on run.

### Diminishing returns reached

Remaining profile: quick-xml parse core (read_event/peek_one/memchr/emit_start)
~27%, allocator ~12% (mostly per-row `PyDict_New` + output PyUnicode values --
irreducible, they ARE the output), CPython iterator/eval machinery ~7%.

Options beyond this point (both are API/architecture changes, not tweaks):

- **`__next_batch__(n)`** returning a list of dicts: cuts Python iterator
  dispatch, est ~3-4%.
- **Custom row-tag scanner** replacing quick-xml's generic event model
  (memchr straight to `<Row`): the only way to attack the 27% parse core.

## Round 5 (mmap + presized dict + PGO)

Three more levers, benchmarked individually (100 MB, median of 3+ runs):

1. **mmap zero-copy input** -- file is memory-mapped (memmap2) and parsed via
   `Reader<&[u8]>::read_event()` with borrowed events. Eliminates BufReader
   `fill_buf` (5.7%), the event copy buffers, and their clears entirely.
   0.409s -> 0.382s (~7%). Zero-length files fall back to an empty slice
   (Windows cannot map empty files).
2. **Presized row dict** -- `_PyDict_NewPresized` via `pyo3::ffi`, sized to the
   previous row's width (rows are homogeneous). Avoids CPython dict rehashing
   during insertion. 0.382s -> 0.359s (~6%).
3. **PGO (profile-guided optimization)** -- two-phase build with
   `-Cprofile-generate` / `-Cprofile-use`, workload = benchmarks.py. See
   `pgo-build.ps1`. **0.359s -> 0.244s median (~33%)** -- by far the largest
   single win of any round; the parser's branchy event loop benefits massively
   from real branch/inline data.

Result: 100 MB parse **~0.24s, 414 MB/s, 375k rows/s**.
Cumulative vs 0.56s baseline: **2.3x faster**.

Note: a plain `maturin develop --release` produces a non-PGO binary (still
~0.36s). Use `pgo-build.ps1` for release artifacts.

## Round 6 (post-merge full sweep: stream intern revival, FxHash, batch iteration)

VTune (sw sampling) on both engines after the upstream columnar merge, plus a
code review pass. Changes, speed-first per direction:

**Stream engine** (was 0.73s after merge):
1. Reader split into pure-Rust `RowParser` + Py-side `CrxmlReader` so
   `py.allow_threads` only captures the Ungil-safe parser -- this re-enables
   the interned-key cache (`FxHashMap<String, Py<PyString>>`) the upstream
   rewrite had to drop. Keys are interned once; every row dict reuses them.
2. `_PyDict_NewPresized` (targeted `#[allow(unsafe_code)]`, matching the
   crate's mmap style) -- rows are homogeneous width, no rehash.

**Columnar/parallel engine**:
3. All field-dispatch and plan maps switched SipHash -> FxHashMap (SipHash was
   ~7-9% of parallel CPU).
4. Killed per-element `child_name.as_ref().to_vec()` end-tag copies -- the tag
   is statically known (`b"Field"`/`b"Text"`) in those branches.
5. Dictionary encoding got a value->code side-index (was O(n) linear scan per
   value, quadratic overall); chunk merge remaps dictionaries once instead of
   per-code, and no longer clones `column_order`.
6. Dead deps removed: indexmap, bumpalo (declared, never used).

**Python layer**:
7. `_arrow_iter`/`_iter_batches` use `table.to_batches()` + `to_pylist()`
   instead of per-cell `column(col)[i].as_py()` -- columnar row iteration went
   from 3.1s / 2.8s to ~1.1s / 0.8s and no longer materializes every row at once.
8. `use_mmap` defaults True; `columnar`+`mmap` features added to the default
   maturin build (non-mmap paths read the whole file into a Vec: ~110 MB
   retained RSS on the 100 MB benchmark).

**Results (100 MB, PGO build via pgo-build.ps1):**

| Path | Before round | After (PGO) |
|---|---|---|
| stream dict iteration | 0.73s | **0.31-0.34s (~290k rows/s)** |
| parallel -> Arrow/DataFrame | 0.13s | **0.126-0.13s (~780 MB/s)** |
| columnar row iteration | 3.1s | ~1.1s |
| parallel retained RSS | 283 MB | ~172 MB (mmap default) |

VTune caveat: HW sampling unavailable on this CPU ("cannot recognize
processor"); software sampling used, attribution call-stack-accurate.

## Round 7 (1 GB/s push: zero-copy parse, move-merge, parallel batch export)

Goal: >=1 GB/s on the parallel columnar path (was ~780 MB/s).

1. **Borrowed-slice parsing** -- `parse_bytes` switched from
   `Cursor + read_event_into` (every event memmoved into a scratch buffer) to
   `Reader<&[u8]>::read_event()`: events reference the chunk bytes directly,
   zero copies, no scratch buffers. Values flow as `Cow<str>` all the way to
   the builders via `push_field_str` (String columns copy once into storage;
   typed columns parse without ever allocating; Dictionary allocs only for
   new dictionary entries).
2. **`push_field_str` fast path** -- no rename/drop configured means no plan
   lookup and no owned key copy per field (was 1 String alloc per field/row).
3. **Move-merge** -- `extend_owned` uses `Vec::append` (pointer moves) instead
   of cloning every value serially across 24+ chunks.
4. **Parallel batch export** -- `engines_to_pyarrow_table`: each chunk engine
   becomes an arrow RecordBatch built in parallel off-GIL; the table's columns
   arrive chunked via `pa.Table.from_batches` -- the serial merge + serial
   arrow re-copy is gone entirely. (auto_dict still merges first; dictionary
   columns get `combine_chunks` so per-chunk dictionaries unify.)
5. **Chunk default 2x threads** -- finer chunks even out per-chunk variance
   (measured ~15% on 24 cores).

Result (100 MB, PGO build, warm best-of-5 on the core call):

| num_chunks | time | throughput |
|---|---|---|
| 24 | 0.0852s | 1173 MB/s |
| **48 (new default)** | **0.0835s** | **1198 MB/s** |

**Parallel columnar core: ~1.2 GB/s, ~1.08M rows/s.** Harness single-shot
runs (cold rayon pool, per-run process) land 790-1010 MB/s.
All 32 Rust + 116 Python tests pass.

## Next Optimizations (if still needed)

### Structural

1. **Pre-allocate `PyDict` capacity** — `PyDict::new(py)` creates an empty dict. CRXML rows typically have 10-30 fields. Setting `dict.min_capacity(32)` or similar before insertion avoids Python dict rehashing. (Check if PyO3 exposes `PyDict_NewPresized`.)

2. **Batch dict insertion** — Use `PyDict::from_sequence` with a pre-built list of `(key, value)` tuples to reduce GIL boundary crossings. Trade-off: requires building the intermediate list.

3. **Skip GIL re-acquisition** — Our `__next__` holds `py` from the top but each `dict.set_item` call is a Python C API call. Investigate `PyDict::set_item` overhead vs raw `ffi::PyDict_SetItemString`.

### File I/O

4. **Larger BufReader** — Currently 128 KB. On NVMe, 512 KB or 1 MB may reduce `fill_buf` overhead at the cost of memory. (Minor — I/O is only ~7.7%.)

### quick-xml tuning

5. **Disable encoding detection** — `quick-xml` has a `ReaderBuilder::encoding` feature. If we know input is UTF-8, skipping encoding sniffing saves CPU in `emit_start`/`decode_cow`.

6. **Custom entity resolution** — `unescape_value()` and `unescape_text()` allocate even for inputs without entities. quick-xml supports `unescape_value_with` with a custom resolver; a no-op resolver for CRXML (no entities in field values) would avoid copies.

### Python-side

7. **Batch row creation** — The Python `CrystalXMLSource.__iter__` calls `__next__` once per row. For extremely small rows, the per-call overhead dominates. A `__next_batch__(n)` method returning a list of dicts would amortize Python call overhead.
