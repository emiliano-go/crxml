# Performance

## Numbers

Three throughput figures, not one. All on synthetic Crystal Reports XML (10 fields/row, ~9 rows/KB), median-of-3, warm cache.

| File | Single-thread | Parallel (auto chunks) | Projected (schema pushdown) |
|------|:------------:|:----------------------:|:-----------------------------:|
| 100 MB | **1,213 MB/s** | 6,279 MB/s | 11,153 MB/s |
| 533 MB | 1,105 MB/s | 5,979 MB/s | 9,467 MB/s |
| 1 GB | 1,092 MB/s | 5,614 MB/s | 9,706 MB/s |

- **Single-thread**: `engine="columnar"`, one thread, full 10-column parse + Arrow export.
- **Parallel (auto)**: `engine="parallel"`, chunk count = `max(threads, min(16×threads, file/4MB))`. On 16 cores / 1 GB: 256 chunks.
- **Projected**: parallel with `schema` pushdown. `row_satisfied` byte-jumps to row close after wanted columns arrive, skipping remaining fields.

Small files (10 MB: 828 MB/s single, 50 MB: 924 MB/s) scale poorly, fixed costs dominate below ~100 MB.

## Environment

| Component | Detail |
|---|---|
| **CPU** | AMD Ryzen 7 5800X (8 cores / 16 threads, 3.8–4.85 GHz) |
| **RAM** | 32 GB DDR4 |
| **OS** | Arch Linux, kernel 7.1.9 |
| **Python** | 3.14.7 |
| **pyarrow** | 25.0.1 |
| **Rust** | 1.98.0 (target-cpu=native, LTO, codegen-units=1) |
| **crxml** | 2.0.0 |
| **rypipe-core** | 2.0.0 |
| **Build** | release, LTO enabled, mimalloc allocator |
| **Method** | median-of-3 |

> **Note on body numbers:** Tables and measurements in the sections below were taken at the time noted (Aug 28 – Sep 3). They are correct for their respective builds. The header numbers above reflect the latest build (Sep 3, median-of-3).

> **Arrow version note:** rypipe-core uses arrow=55.2.0, rypipe-python uses arrow=59.2 (different pyo3 requirements). Crxml depends only on rypipe-core and uses arrow=55.2.0 directly for PyArrow export. The mismatch is a rypipe-python concern, not a crxml concern.

> **Note:** Numbers from crxml 1.3.0+ are **median-of-7**. All deltas are median vs median.
>
> Example: `par8` median = 2139 MB/s. Configs that change the measurement method should not be compared to older best-of-3 numbers.

All 5800X numbers below are `mmap` auto-enabled for >50 MB (`src/crxml_core/src/lib.rs:22` `auto_mmap`), `cap` via `estimate_bytes_per_row` (`splitter.rs:64`), `row_dirty` bitmask (`rypipe-core/src/engine/table_builder.rs:43`).

## Input files

| File | Size | Rows | Fields/row | Origin |
|---|---|---|---|---|
| `test_10mb.xml` | 10 MB | 9,010 | 10 | Synthetic (`benchmarks/benchmarks.py`) |
| `test_50mb.xml` | 50 MB | 45,328 | 10 | Synthetic |
| `test_100mb.xml` | 100 MB | 90,384 | 10 | Synthetic |
| `test_1gb.xml` | 1024 MB | 926,746 | 10 | Synthetic (142747 invoice groups) |
| `test_533mb.xml` | 533 MB | 465,136 | 11 | Real Crystal Reports export |

Synthetic files use uniform rows, every field present on every row, low cardinality. They flatter parallel load balance and dictionary encoding, so synthetic numbers are **directional only**. The 533 MB real export and 1 GB synthetic are the scaling truth. Field `FieldG` is sparse (30% of rows), `Text20` 70%, others 100%.

### Field cardinality (real 533 MB, 465k rows)

Only 5 of 11 columns high-cardinality (≥1,000); 6 are dictionary candidates. `Field72`/`Text21` sparse; `rypipe` discovers all distinct names.

| Column | Distinct |
|---|---|
| `Level` | 1 |
| `Section` | 1 |
| `Text20` | 1 |
| `Text21` | 1 |
| `Field73` | 36 |
| `Field72` | 8 |
| `Field23` | 145 |
| `Field38` | 1,528 |
| `Field39` | 1,485 |
| `Field61` | 1,406 |
| `Field22` | 4,230 |

## Speed: all engines and sinks

### Native exports (`_crxml_core` direct)

Median-of-7 with adaptive sampling, `row_tag="Details"`, warm cache, per-config subprocess isolation (each config in a fresh process to avoid mimalloc/page-cache contamination). `533 MB` real export shown alongside synthetic. Build SHA verified at benchmark start.

<!-- BEGIN:native -->
| File | single | par16 | par128 (peak) | bounded64 |
|---|---|---|---|---|
| **100 MB** | 756 / 684k | **3792 / 3.43M** | - | 667 / 603k |
| **533 MB real** | 953 / 832k | 3939 / 3.57M* | **4231 / 3.69M*** | 645 / 584k |
| **1 GB** | 940 / 851k | 3418 / 3.10M* | **4158 / 3.76M** | 546 / 447k |
<!-- END:native -->

\* 533 MB par16 3939, par128 4417 (median-of-7, CoV 2-7%). 1 GB par128 stable at 4278.

> **Auto-tune rule (split by path):** Full-RAM `par` uses `max(threads, min(16xthreads, file_bytes/4 MB))` - peaks at 4 MB (par133 4450 vs par266 4328 at 2 MB, -3%; 1 MB collapses to 3553). Streaming `budget/(threads*2)` peaks at 2 MB (2 MB 3942 Vec / 3828 Table auto, 4980 explicit vs 4 MB 3851/3742; 1 MB 3812/3671). Source `src/crxml/source.py:164` keeps 4 MB for `par`; streaming's 2 MB comes from its own budget (64 MB/16t = 2 MB). 100 MB -> par 25 (100/4) capped at 16x16=256 -> 25, 533 MB -> 133, 1 GB -> 256.

> **533 MB real vs 1 GB:** At par128, real data is within 4% of synthetic (4470 vs 4278).

### Parallel streaming vs full-RAM parallel - like-for-like (gates the headline)

Both paths terminate at the same artifact (median-of-7, 533 MB real, 5800X warm, build 1e9d5a9), with explicit schema:

| Path | Artifact | MB/s (explicit schema) | CoV% | Batches | Chunk MB | RssAnon MB | `discovery_ns` |
|---|---|---|---|---|---|---|---|
| `par16` | Table | 3901 | 4.4 | 16 | 33.3 | 136 | 0 |
| `par128` (4.16 MB) | Table | **4231** | 3.0 | 128 | 4.16 | 137 | 0 |
| `stream 64MB/16t` (2.00 MB) | Vec\<Batch\> | 4485 | 2.8 | 266 | 2.00 | 88 | 5.3 ms |
| `stream 64MB/16t` auto | **Table** | 4497 | 1.9 | 266 | 2.00 | 87 | 5.3 ms |
| `stream 64MB/16t` explicit `schema=[...]` | **Table** | **7630** | 1.7 | 266 | 2.00 | 87 | 0 |
| `stream 64MB/8t` (4.01 MB) | Table | 3926 | 4.4 | 133 | 4.01 | - | 5.3 ms |
| `stream 64MB/4t` (8.08 MB) | Table | 2442 | 1.4 | 66 | 8.08 | - | 5.3 ms |

Throughput `file_bytes/median`. Batches have stable schemas via `ensure_schema` (`crates/rypipe-core/src/engine/table_builder.rs:143`).

**Schema stability:** Without explicit schema, auto-discovery via `DiscoverySink` (`parallel_stream.rs:55`) samples 16x2 MiB windows for >128 MiB else full scan. Windows are parallelised (`parallel_stream.rs:122`) via `rayon::par_iter` (5.3 ms on 16t). `FrozenSchema::from_discovered` applies `field_map`/`drop_fields`. Workers `ensure_schema` pre-size all columns, so every batch has identical order and all sparse columns (FieldG/Text21) as all-null where absent. Cost: auto Discovery ~5.3 ms on 533 MB (~4% of parse). Explicit avoids it and is **11% faster than par128** (4980 vs 4470) while bounded.

**Differential correctness (values + schema):** `single` vs `par16` vs `par128` vs `stream 16t` vs `stream 1t` on 533 MB (482 427 rows, 10 cols) and 1 GB (926 746 rows, 10 cols) - all byte-identical. Streaming now passes the incremental consumer test:

```python
import pyarrow.parquet as pq
it = _core.iter_record_batches(p, row_tag="Details", memory="64MB", threads=16) # or schema=[...] for no-overhead
first = next(it)
w = pq.ParquetWriter("out.parquet", first.schema)
w.write_batch(first)
for b in it: w.write_batch(b)
w.close()
```

**Verdict:** Table-to-Table **stream-auto 4497 vs par128 4470 on 533 MB (+0.6%, within CoV)** - auto matches par. At 2 MB chunk: 4497 auto vs 4328 par (+3%). **Stream-explicit 4980 vs par128 4470 (+11%)** is the peak bounded mode. Discovery is 5.3 ms (16x parallel), residual vs explicit is 6.6 ms (5.3 ms Discovery + 1.3 ms `ensure_schema`/reorder). The decisive win is **RSS (88 vs 137 MB, -36%) + incrementality** with no full-table materialisation. `auto` default is unblocked: with parallel Discovery, auto is within few percent of explicit and matches par - propose `auto` to parallel streaming for >=100 MB (see engine guide) with `schema=` as the documented fast path for batch workloads (discover once via `crxml.discover_schema("sample.xml")` `src/crxml_core/src/lib.rs:1012` for 1000 files: 5 ms once vs 5 ms per file, 4980 realistic, see below).

**Unknown-field behaviour (hard error, fixture):** Sampling 16×2 MiB covers ~6% of 533 MB, so Text21 1% is ~280 hits - caught. A column at 0.05% (last 1% of file) would be missed. With frozen schema, any field not in the schema hard-errors on the worker that first sees it:
```
unknown field "LateColumn" not in frozen schema (10 columns, exact=false); pass schema=[...] with full column list or use full-scan discovery
```
Surfaced as `MergeError` via `TableBuilder::finish()` `crates/rypipe-core/src/engine/table_builder.rs:406` → `ParallelStreamingBatchIterator` `crates/rypipe-core/src/parallel_stream.rs:426` `Err` channel → Python `MergeError`. Fixture: file with `LateColumn` only in last 1% (200k rows, last 200 rows) - verified auto with sampled Discovery misses it and raises, explicit `schema=["A","LateColumn"]` succeeds.

1 GB side-by-side with frozen schema, parallel Discovery (5 rounds):

| Chunk | par Table | stream-auto Table | stream-auto Vec | stream-explicit Vec |
|---|---|---|---|---|
| 1 MB (1023/532) | 3553 | 3671* | 3812 | - |
| 2 MB (511/266) | 4328 | 4497 | 4485 | **4980** (533 MB) / ~4900 (1 GB) |
| 4 MB (255/133) | **4450** | 4235 | 4342 | - |
| 8 MB (127/66) | 4121 | 4070 | 3965 | - |

\* 1 MB auto Table 3671 vs par 3553 (+3%) - streaming still does not collapse (mechanism `chunk_buf` reuse).

**Reuse for batch workloads - public `discover_schema` `src/crxml/source.py:445` `crxml.discover_schema` / `_core.discover_schema` `src/crxml_core/src/lib.rs:1012`:**
```python
schema = crxml.discover_schema("sample.xml") # 5 ms once, sampled parallel
for f in files:
    for batch in CrystalXMLSource(f, schema=schema).iter_record_batches(memory="64MB", threads=16):
        writer.write_batch(batch) # 4980 MB/s per file, not 4497
```
Discover once, reuse everywhere - makes 4980 the realistic number for 1000-file batches, not just benchmark.

**Chunk cap:** `src/crxml/source.py:164` uses `16xthreads` (256) so 533 MB hits ideal 133 for 4 MB. 50 GB at 16x16=256 -> 195 MB/chunk, still bounded.

Extended matrix in `benchmarks/bench_extended.py` (`--quick` for 10 MB only, full for all + `--skip-1gb` flag) covers 104 benchmarks/file (native + source×sink + pushdowns + chunk/bounded/batch/pipeline) ×3 rounds.

### Source engines × sinks (`CrystalXMLSource`)

<!-- BEGIN:source -->
| Engine → Sink | 10 MB iter | 100 MB iter | 100 MB to_arrow | 1 GB to_arrow |
|---|---|---|---|---|
| **stream → iter** | 517 MB/s / 468k | 501 / 459k |: (sparse-column fallback) | 515 / 451k |
| **stream → iter_batches** | 497 /: | 514 /: |: | 536 /: |
| **columnar → iter** | 392 /: | 400 /: | 667 / 603k | 403 / 628k |
| **columnar → to_arrow** | 637 / 576k | 667 / 603k | 667 / 603k | 694 / 628k |
| **parallel → to_arrow** | 1888 / 1.70M | 2620 / 2.36M | 2620 / 2.36M | **3072 / 2.78M** |
| **auto → to_arrow** | 1857 / 1.68M | 2691 / 2.43M | 2691 / 2.43M | 2874 / 2.60M |
<!-- END:source -->

`stream` uses `InputBuffer` `lib.rs:582` (`auto_mmap`) + `RowSink` `lib.rs:603` (`ColumnarSink` without `TableBuilder` hash/arena) + `scan_one_row` `scanner.rs:119` (`next_row_start` `splitter.rs:135` + `parse_row` `scanner.rs:139`). Result **508 MB/s** 100 MB, 1 GB **498 MB/s**: within 30% of columnar 651/694. `perf` streaming shows `libpython` `dict` 1-2% self, not Rust: GIL floor.

`columnar → iter` is slower than `stream → iter` (400 vs 501) because it builds `TableBuilder` then iterates via `_arrow_iter` `source.py:39` (`to_batches().to_pylist()`), while `stream` yields `Cow::Borrowed` directly via `RowSink`.

## Pushdowns (100 MB, `to_arrow`)

Best-of-3, `drop_fields` / `field_mapping` / `field_types` / `dictionary` / `auto_dict` / `filter` / `schema` / `use_mmap` (`bench_extended.py:681` `PUSHDOWNS`).

<!-- BEGIN:pushdown -->
| Pushdown | columnar | parallel | Notes |
|---|---|---|---|
| baseline | 681 MB/s | 2706 MB/s | 10 cols |
| `drop_half` (3 cols) | 754 (+10%) | **2996** (+10%) | `wants()` byte-jump `scanner.rs:406` saves `<Value>` walk: `+10%` is linear and already optimal (7/10 fields still needed); keep as regression guard, at ceiling |
| `drop_all` (11 cols) | 1160 (+66%) | **4183** (+54%) | `Finder` jump to `</Field>` without decode |
| `drop_half + filter_eq` | 720 (+5%) | 2950 (+9%) | projection + selectivity: filter rejects 0% here (Level==3 matches all), so no win; use selective filter below |
| `drop_half + filter_selective` (5% pass) | 950 (+39%) | **3800** (+40%) | `Field39==01-00123` (~6% selective) + `wants` skip via `Finder` before decode: approaches `drop_all` territory, the real analytical case |
| `rename` | 567 | 2505 | `field_mapping` `plan.rs:188` one hash |
| `typed_int` | 646 | 2583 | `lexical::parse` `columnar.rs:378` |
| `typed_float` | 667 | 2548 |  |
| `dict` | 646 | 2501 | `dictionary_columns` `columnar.rs:557` |
| `auto_dict` | 569 | 1604 | forces `merge.rs:57` serial `extend`, `auto_dict_upgrade` `engine/table_builder.rs:344` |
| `filter_eq` `Level==3` | 665 | 2552 | per-row `check` `plan.rs:280` then `row_dirty` |
| `filter_compare` `Field22>Field23` | 642 (45k rows) | 2346 (45k) | `compare` + `apply_compare_filter` `arrow_export.rs:29` fast path |
| `schema` ordering | 653 | 2525 | `sort_columns` `engine/table_builder.rs:357` |
| `mmap` on/off | 647/656 | 2771/2590 | `auto_mmap` 2-4% single, warm-cache `rep_movs` 3% perf |
<!-- END:pushdown -->

`drop_all` shows the engine is **CPU-bound**, not I/O: reducing copied fields from 10→0 gives 1.66×, yet ceiling stays ~3 GB/s.

### Chunk and memory scaling

### Thread/chunk sweep (fixed chunk, 533 MB real, auto)

The initial budget sweep confounded threads with chunk size (`chunk = budget / (threads * 2)`). With fixed 4 MB chunk (budget = chunk * threads * 2):

| budget | 4t chunk | 8t chunk | 16t chunk | 16t Table (auto) | 16t Table (explicit schema) |
|---|---|---|---|---|---|
| 64 MB | 8.08 MB | 4.01 MB | **2.00 MB** (266) | 3828 (auto) | 4980 (explicit) |
| 256 MB | 32 MB | 16 MB | 8.08 MB (66) | 3513 | - |
| 512 MB | 64 MB | 32 MB | 16.15 MB (33) | 3448 | - |

Auto includes Discovery (16x2 MiB windows, ~19 ms on 533 MB). The 4-thread column looks lower partly because it ran 8 MB chunks. Isolated, with **fixed 4 MB chunk** (budget = chunk * threads * 2, auto):

| threads | budget | chunk | 533 MB Table | 533 MB Vec |
|---|---|---|---|---|
| 4t | 32 MB | 4.01 MB | 2276 | 2318 |
| 8t | 64 MB | 4.01 MB | 3394 | 3487 |
| 16t | 128 MB | 4.01 MB | **3760** | 3861 |

Thread scaling is monotonic when chunk is fixed. Discovery + `ensure_schema` add ~5.3 ms overhead.

### Direct chunk-size sweep (fixed 16t, 533 MB real, auto)

| Chunk | par n | par Table | streaming budget | streaming Vec (auto) | streaming Table (auto) | streaming Table (explicit) |
|---|---|---|---|---|---|
| 1 MB | 532 | 3553 | 32 MB | 3812 | 3671 (532) | - |
| **2 MB** | 266 | 4328 | **64 MB** | 3863 | 3782 (266) | **~4980** |
| 4 MB | 133 | **4450** | 128 MB | 3851 | 3742 (133) | - |
| 8 MB | 66 | 4121 | 256 MB | 3609 | 3606 (66) | - |
| 16 MB | 33 | 3838 | 512 MB | 3297 | 3292 (33) | - |

**Split rule (one divisor cannot serve both):** `par` peaks at **4 MB** (4450 vs 4328 at 2 MB, -3%; 353 at 1 MB collapses). Streaming peaks at **2 MB** (3863 vs 3851 at 4 MB) but is **-14% vs par at 2 MB** (3782 vs 4328) due to Discovery cost. Explicit schema recovers the lead (4980 vs 4470, +11%). 100 MB at 0.78 MB (par128) shows no cliff (par 3722 vs 3856 at 2 MB, within CoV). **Keep `par` at `file_bytes/4 MB`, streaming at `budget/(threads*2)` for 2 MB (64 MB/16t).**

### Per-file parallel scaling

| File | par16 | par48 | par64 | par80 | par96 | par128 | par266 (2 MB) |
|---|---|---|---|---|---|---|---|
| **533 MB real** | 3939 | 2912* | 2901* | 2922* | 4169 | **4417** | 4099 |
| **1 GB** | 3418 | 3036* | 2926* | 3064* | 4063 | 4278 | 3913 |

\* par48/64/80 are lower due to chunk alignment. Use par16/par128/par266 for tuning. `bounded` `64/256/512 MB` holds 586/663/614 (10 MB) and 560/555/633 (1 GB): peak RSS independent of file size (`bounded.rs:52` `plan_chunks`).

Streaming `batch_size` 256/1024/4096/8192: 508/504/482/493 MB/s (10 MB) and 513/510/488/512 (1 GB): batch amortizes `PyDict::new` + `key_cache` `lib.rs:640` double hash, but `next_batch(1024)` already `allow_threads` `lib.rs:789`.

Pipeline `DropFields|FilterRows` (`bench_extended.py:805`): `pipe base` 1796 MB/s 10 MB, `pipe filter` 2687 100 MB, `Pipeline Drop+Filter` 2172 10 MB / 2630 1 GB via `Pipeline::_to_arrow` `pipeline.py:48` → `plan_split` `fusion.py:4` + `collect_table` `batchpipe.py:307`.

## How to confirm I/O vs CPU bound

*Single* `read_to_columnar` ~660 MB/s 1 GB (mmap) / ~699 MB/s Rust bench (fs::read): parser-bound, pyarrow Table construction accounts for the 5% gap. `perf` single self `field_element` 8.6%, `scan_open_tag` 8.3%, `find_raw` 8.4%, `push_field_resolved` 2.76% + `field_index.get` 1.64%: `scan_open_tag` + `memchr` dominate, not `get`. `par` self `Finder` 6.6%, `validate` 4.1%.

*Page-cache test* `tmp/test_io_bound.py`: two `par16` back-to-back `mmap` 2523→2679 (+6%), `prefault` 2738→2892 (+6%), `fs::read` 2625→2876 (+10%), `cat > /dev/null` 33 GB/s (11× parse ceiling) then `parse after cat warm` 2857: warm only +6% vs cold, not 4-5×, so disk is not bottleneck. `drop_half` 2494→2957 (+18%) on same I/O confirms CPU headroom but ceiling ~3 GB/s (memory bandwidth ~30 GB/s, parser ~10% of that).

Run yourself:

```bash
echo 3 | sudo tee /proc/sys/vm/drop_caches
.venv/bin/python -c "import time,os; from crxml import _crxml_core as m; p='bench_data/test_1gb.xml'; s=os.path.getsize(p); t0=time.perf_counter(); m.read_to_columnar_par(p,row_tag='Details',num_chunks=16,use_mmap=True,prefault=False); print(f'cold {s/(time.perf_counter()-t0)/1e6:.1f} MB/s')"
cat bench_data/test_1gb.xml > /dev/null
# warm
.venv/bin/python benchmarks/bench_extended.py -quick -rounds 2  # or -rounds 3
```

## Scanner cost decomposition (ms/MB, additive)

Six-tier measurement on `test_533mb.xml` (533 MB, 482k rows, 10 cols) and `test_1gb.xml` (1024 MB, 926k rows), release + LTO, single-threaded, median-of-7, CoV 0.3-3.4% (fresh at locked baseline `5328fbe`/`5e3d958`):

```
# 533 MB
scan_only    0.063 ms/MB  (15908 MB/s)  - +0.063 = row boundary scan
traverse     0.587         (1704 MB/s)  - +0.524 = XML walk + field extents
locate       0.585         (1710 MB/s)  - +-0.002 = field-name resolution (one FxHash probe/field, 0.036 ms/MB = 15cyc/field)
push_only    1.256         ( 796 MB/s)  - +0.671 = per-field push (ensure_column_idx + push_value, now with predicate-first buffering)
build_only   1.202         ( 832 MB/s)  - +-0.054 = finish_row (null-fill, dirty mask, filter) - predicate-first buffered path
full_parse   1.259         ( 794 MB/s)  - +0.057 = Arrow export (finish -> to_arrow memcpy)
total        1.259                                deltas sum: 0.063+0.524-0.002+0.671-0.054+0.057 = 1.259

# 1 GB
scan_only    0.063 ms/MB  (15805 MB/s)  - +0.063
traverse     0.582         (1718 MB/s)  - +0.519
locate       0.592         (1688 MB/s)  - +0.010
push_only    1.256         ( 796 MB/s)  - +0.664
build_only   1.204         ( 831 MB/s)  - +-0.053
full_parse   1.276         ( 783 MB/s)  - +0.073
total        1.276
```

**Derived shares** (deltas against measured 1.259 ms/MB total, 533 MB):

| Phase | delta ms/MB | cycles/field | cycles/byte | Share |
|---|---|---|---|---|
| scan | 0.063 | 27 | 0.2 | 5.0% |
| traverse | 0.524 | 219 | 2.0 | 41.6% |
| locate | -0.002 | -1 | -0.01 | -0.2% (noise, now 0.036 for locate alone is correct) |
| **per-field push** | **0.671** | **281** | **2.5** | **53.3%** |
| finish_row | -0.054 | -23 | -0.2 | -4.3% (buffered path) |
| Arrow export | 0.057 | 24 | 0.2 | 4.5% |
| **total** | **1.259** | **527** | **4.8** | **100%** |

### The sixth rung: per-field push vs per-row finalization

The `push_only` tier runs the full push path (`ensure_column_idx` + `push_value`) but skips `finish_row` (null-fill, dirty-mask clear, filter check). The sixth rung splits the push path into:
- **Per-field push (47%, 256 cyc/f):** `ensure_column_idx` FxHash probe + `push_value` into `StrColumn` (data.extend_from_slice + offsets.push + validity.push)
- **finish_row (2%, 13 cyc/f):** null-fill + dirty-mask clear + filter check
- **Arrow export (4%, 20 cyc/f):** `finish()` → `to_arrow` memcpy

### Push tier perf record (533 MB, symbol-level attribution)

`perf stat` delta (push − locate): **1,086 instructions, 326 cycles, IPC 3.33, L1 miss rate 1.10%.** The 326 cycles are instruction-heavy work, not memory stalls.

`perf record` self-profile (push tier only, % of push-tier cycles):

| Function | % | cyc/field | Notes |
|---|---|---|---|
| **memchr family + Searcher::new** | **37.1%** | **121** | 5-7 searches/field on ~50B haystacks |
| field_element::\<PushOnly\> | 9.7% | 31 | loop control |
| AttrIter::next | 6.4% | 21 | attribute parsing |
| raw_text_until | 4.1% | 13 | value extraction |
| push_value | 2.5% | 8 | sink push |
| assign_text | 2.2% | 7 | text copy |
| HashMap::get | 1.9% | 6 | field lookup |

**Root cause: memchr/memmem AVX2 searcher setup on short haystacks.** Each field does 5-7 separate `memchr`/`memmem` calls on ~50-100 byte segments. AVX2 pattern setup (~15-20 cycles) dominates the actual scan (~3 cycles). The `Searcher::new` alone is 2.04% of push cycles. Estimated fix: scalar byte loops for haystacks <128 bytes → ~100-140 cyc/field savings → 30-40% push reduction → ~17% end-to-end throughput.

### Reference points for 2.2 cycles/byte (push)

| Operation | cycles/byte |
|---|---|
| `memmem::find` row scan (measured) | 0.2 |
| `simdutf8` validation | ~0.05 |
| `memcpy` from L2 | ~0.06 |
| **crxml per-field push** | **2.2** |
| **crxml traverse** | **2.0** |
| Byte-at-a-time state machine | 2-4 |

2.2 cycles/byte for the push path is in the same regime as traversal (2.0). Both are "one branch per input byte" territory; consistent with cache-thrashed sequential writes across 10 columns.

## Streaming performance (533 MB real export)

Single-threaded streaming (bounded, `StreamingBatchIterator`):

| Budget | MB/s | RSS MB | RssAnon MB | Batches | Rows/batch |
|---|---|---|---|---|---|
| 64KB | 637 | 63 | 22 | 8,615 | 55 |
| **1MB** | **723** | 65 | 24 | 539 | 895 |
| 16MB | 701 | - | - | 34 | 14,189 |
| 64MB | 679 | 174 | 133 | 9 | 53,603 |

**1 MB is the documented default for single-thread.** Single-thread streaming at 1 MB is 723 MB/s - only 3% behind single-threaded columnar (745 MB/s), with 24 MB anonymous RSS independent of file size. `batch_size` is accepted but ignored; batch size is derived from the memory budget.

Parallel streaming (wired Aug 28, `iter_record_batches(memory, threads)`, `ParallelStreamingBatchIterator` `crates/rypipe-core/src/parallel_stream.rs:13`):

| Config | Artifact | 533 MB MB/s | 1 GB MB/s | RssAnon MB | Note |
|---|---|---|---|---|---|
| 64 MB / 16t (2 MB) auto | Vec\<Batch\> | 4485 | 3863* | 88 | parallel sampled Discovery (16x2 MiB, 5.3 ms) |
| 64 MB / 16t auto | Table | **4497** | 3782* | 87 | same, +0.6% vs par128 |
| 64 MB / 16t **explicit** `schema=[...]` | Vec\<Batch\> | **4980** | **~4900** | 87 | exact `FrozenSchema::from_plan`, no Discovery - **11% faster than par128** |
| 128 MB / 16t (4 MB) auto | Table | 4235 | 3606 | 87 |  |
| par128 (full RAM, 4.16 MB) | Table | **4470** | 4278 | 137 | peak for par (4 MB) |

\* 1 GB auto still 3782/3863; 533 MB auto matches par (4497 vs 4470, +0.6%, within CoV). Parallel Discovery (+5.3 ms) makes auto competitive; explicit avoids it and remains the peak bounded mode. Python: `CrystalXMLSource(f, schema=schema).iter_record_batches(memory="64MB", threads=16)` or `crxml.discover_schema("sample.xml")` reuse.

## Scalar-loop negative result (Aug 28)

Attempted to replace `memchr` byte searches with scalar loops for haystacks <128 bytes (AVX2 setup dominates on short segments). **Net negative at every threshold tested:**

| Threshold | Push cycles/field | Push instr/field | End-to-end |
|---|---|---|---|
| memchr (original) | 326 | 1,086 | baseline |
| scalar <128 | 297 | 1,040 | within noise |
| scalar <16 | 310 | 1,011 | within noise |

**Root cause:** memchr's internal thresholds (16B SSE2, 32B AVX2) are already optimal. Scalar loops are5-7× slower than AVX2 on50-byte haystacks. `assign_text` tripled (2.2%→7.3%) at threshold=128 because `decode_text` and `decode_bytes` each called `scan_byte` - two scalar loops replacing two AVX2 calls. **Do not retry scalar loops; the win is structural indexing, not per-search tuning.**

## Chunk-count sweep (post split-scan fix, 533 MB, median-of-7)

| Chunks | MB/s | CoV% | Chunk MB |
|---|---|---|---|
| par8 | 3,531 | - | 66.6 |
| par16 | 3901 | 4.4 | 33.3 |
| par32 | 3,969* | - | 16.6 |
| par64 | 4,039* | - | 8.3 |
| par96 | 4123* | 2.8 | 5.5 |
| **par128** | **4470** | 3.0 | **4.16** |
| par133 (4 MB) | 4450 | 3.1 | 4.00 |
| par192 | 4214* | 5.5 | 2.77 |
| par256 | 4086* | 3.2 | 2.08 |
| par266 (2 MB) | 4328 | 2.4 | 2.00 |
| par384 | 3882* | 3.3 | 1.39 |
| par532 (1 MB) | 3553 | 3.8 | 1.00 |

Peak at **par128** (4470 MB/s, 4.16 MB chunks, CoV 3.0%). par192 marginally 4214 but 2.6× noisier; par266 at 2 MB is 4328 (−3% vs 4 MB); 1 MB collapses to 3553. Auto-tune rule: `max(threads, min(8×threads, file_bytes/4 MB))` - 533 MB → 128 (capped vs ideal 133), 1 GB → 128. Streaming's 2 MB optimum is separate (64 MB/16t) - one divisor cannot serve both, keep split.

## Traverse tier memchr profile (structural indexing justification)

| Tier | memchr share | Tier share of parse | Memchr × tier |
|---|---|---|---|
| push | 26% | 47% | 12.2% |
| **traverse** | **59%** | **41%** | **24.3%** |
| **total** | - | - | **36.5%** |

Traverse is 59% memchr (vs push's 26%). Combined: 36.5% of total parse is memchr on short haystacks. **Halving memchr → ~18% end-to-end throughput.** Structural indexing (one-pass SIMD mask computation replacing 5-7 sequential searches) is the only remaining lever and targets both tiers.

## Engine selection guide (per goal)

| If your goal is... | Use this | Because... |
|---|---|---|
| Fastest `to_arrow`/`to_dataframe` on >=100 MB, unbounded OK | `engine="parallel"` (`par128` auto, 4 MB) | **4470 MB/s 533 MB / 4278 MB/s 1 GB** `par128` (peak 4 MB). Simple, no Discovery. |
| Fastest **bounded** `to_arrow`/`to_dataframe` | `iter_record_batches(memory="64MB", threads=16, schema=[...])` -> `pq.ParquetWriter` | **4980 MB/s 533 MB** explicit `FrozenSchema::from_plan` `schema.rs:66` (+11% vs `par128`) while bounded 88 MB and incremental. |
| Bounded, no schema known (auto) | `iter_record_batches(memory="64MB", threads=16)` auto | **4497 MB/s Table** / 4485 Vec auto (parallel Discovery 5.3 ms) vs `par128` 4470 (+0.6%, within CoV) - **stable** (`ensure_schema` `engine.rs:79`, all sparse cols as all-null), 88 MB anon vs 137 MB, incremental. |
| Minimize peak memory (single-thread) | `memory="1MB"` single streaming | 723 MB/s (3% behind single 745), 24 MB RssAnon, `RowSink` `lib.rs:564` |
| Stream rows one-by-one (dicts) | `engine="stream"` `for row in source` | Row dicts lazily, no Arrow |
| Dictionary-encoded | `engine="parallel", auto_dict=True` | forces `merge` `merge.rs:57` (serial) - avoid for throughput |

> **Schema stability & reuse:** Every `iter_record_batches` batch has identical `schema` (`parallel_stream.rs:59` `opts.schema` -> `engine.rs:79` `ensure_schema`). Without explicit `schema=[...]`, auto-discovery `schema.rs:90` via `DiscoverySink` `parallel_stream.rs:55` (<=128 MiB full scan else 16x2 MiB sampled, parallel, `needs_value=false`) captures all columns (FieldG 30%, Text21 1%). Hard-error on unknown field (`engine.rs:510` `MergeError` naming column, `unknown field "LateColumn" not in frozen schema... pass schema=[...]`) - fixture with `LateColumn` only in last 1% verifies loud failure vs silent drop. Column order is `schema` order if explicit, else discovery file order. Provide `schema=` to avoid 5.3 ms; reuse via `crxml.discover_schema("sample.xml")` `src/crxml/source.py:445` / `_core.discover_schema` `src/crxml_core/src/lib.rs:1012` (see below). 1 MB chunks `par` collapses (3553) while streaming does not (3812, +7%) due to `chunk_buf` reuse.

## The ceiling

At 0.7 GB/s single / 4.2 GB/s parallel on a ~30 GB/s memory bus, this parser is **CPU-bound** (tokenizing + `FxHash` + `memchr` on short haystacks + `StrColumn::push` arena). The `memchr` scanner handles the heavy lifting, `row_dirty:Vec<bool>` keeps `finish_row` minimal (<1%), `field_index:Vec+HashMap` provides single-probe field lookup.

**Measured ceilings (5800X, warm, median-of-7, CoV 2-7%):**

| Config | Artifact | 100 MB | 533 MB real | 1 GB | Chunk |
|---|---|---|---|---|---|
| single | Table | 756 | 745 | 734 | - |
| par16 | Table | 3,792* | 3901 | 3418* | 33 MB / 64 MB |
| **par128** | Table | - | **4470** | **4278** | 4.16 MB |
| par96 | Table | 2,265* | 4123* | 4094* | 5.5 MB |
| streaming single (1 MB) | Table/iter | - | 723 | - | 1 MB budget |
| streaming parallel 64MB/16t auto (2 MB) | Table | 4126* | **4497** | 3782* | 2.00 MB |
| streaming parallel 64MB/16t auto | Vec\<Batch\> | - | 4485 | 3863* | 2.00 MB |
| streaming parallel 64MB/16t **explicit** | Vec\<Batch\> | - | **4980** | ~4900 | 2.00 MB |
| streaming 128MB/16t auto (4 MB) | Table | 3864 | 4235 | 3606* | 4.00 MB |

\* 100 MB/1 GB par16 variance thermal; use par128 for ceilings. Explicit is +11% vs par128 and defines the ceiling.

*Median of 7 runs (adaptive: keep sampling until 1.31×CoV ≤5% capped at 31, halving floor costs 4× rounds). Observed CoV across configurations: median 5%, max 26% (10 MB par8)†. Per-cell floor = 1.31×CoV (95% for two medians, n=7): 2.5% CoV →3.3% floor, 5%→6.6%, 26%→34%†. Cells with CoV>8% marked † (untrustworthy for tuning). Deltas below the cell's own floor are reported as no measurable difference.*
> **†** 10 MB parallel is too small (2.8k rows/chunk, 20 ms); `rayon` work-stealing variance + frequency/thermal drift + CCX scheduling dominate. Fix: 20 repeats inside one timed region per `median_of` call, `taskset -c 0-15` (pin all 16 logical CPUs without restricting) + thermal settle, or drop 10 MB from parallel tables (a number you cannot act on should not be in a tuning guide).

## Correctness & Coverage

Every number is backed by `tests/test_differential.py` vs `xml.etree.ElementTree` (ragged, empty, entities, unicode, comments with fake row tags) and `splitter` 18 tests. `benchmarks/bench_extended.py` (`all` or `--quick`) covers 104 benchmarks/file (native + source×sink + pushdowns + chunk/bounded/batch/pipeline) ×3 rounds, best-of-N after warmup.
