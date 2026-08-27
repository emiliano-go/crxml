# Performance

## Environment

| Component | Detail |
|---|---|
| **CPU** | AMD Ryzen 7 5800X (8 cores / 16 threads, 3.8 GHz base) |
| **OS** | Arch Linux |
| **Python** | 3.12 (venv) |
| **pyarrow** | 24.0.0 |
| **crxml** | 1.2.0 (scanner + `row_dirty` core, super-optimized streaming) |
| **rypipe-core** | 0.1.1 (Vec+field_index+row_dirty) |
| **Build** | release, LTO enabled, mimalloc allocator |
| **Cache** | warm (one warmup parse, best-of-3, variance <5%) |

Previous quick-xml numbers were on i5-1335U. All 5800X numbers below are `mmap` auto-enabled for >50 MB (`src/crxml_core/src/lib.rs:25` `auto_mmap`), `cap` via `estimate_bytes_per_row` (`splitter.rs:41`), `row_dirty` bitmask (`rypipe/src/engine.rs:16`).

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

Best-of-3, `row_tag="Details"`, warm cache.

| File | single | multi2 | par4 | par8 | par16 | par32 | bounded64 | bounded256 |
|---|---|---|---|---|---|---|---|---|
| **10 MB** | 674 MB/s / 610k r/s | 655 / 593k | 1489 / 1.35M | 2154 / 1.95M | 2383 / 2.00M | 2574 / 2.32M | 586 / 531k | 663 / 600k |
| **50 MB** | 587 / 532k | 608 / 552k | 1339 / 1.21M | 1739 / 1.57M | 1913 / 1.73M | 1876 / 1.70M | 522 / 473k | 590 / 536k |
| **100 MB** | 698 / 631k | 674 / 610k | 1747 / 1.58M | 2232 / 2.01M | 2720 / 2.45M | 2619 / 2.37M | 617 / 558k | 612 / 553k |
| **1 GB** | 660 / 559k | 590 / 534k | 1781 / 1.53M | 2485 / 2.10M | 2477 / 2.24M | **2649 / 2.43M** | 546 / 447k | 549 / 456k |

`par32` beats `par16` on 1 GB (32 MB/chunk amortizes `TableBuilder::with_plan` `lib.rs:275`), while 10 MB saturates at `par16` (chunks ~2.8k rows, per-chunk overhead dominates). `bounded` is ~5–10% slower than single but caps RSS at budget.

Extended matrix in `benchmarks/bench_extended.py` (`--quick` for 10 MB only, full for all + `--skip-1gb` flag) covers 104 benchmarks/file (native + source×sink + pushdowns + chunk/bounded/batch/pipeline) ×3 rounds.

### Source engines × sinks (`CrystalXMLSource`)

| Engine → Sink | 10 MB iter | 100 MB iter | 100 MB to_arrow | 1 GB to_arrow |
|---|---|---|---|---|
| **stream → iter** | 517 MB/s / 468k | 501 / 459k | — (sparse-column fallback) | 515 / 451k |
| **stream → iter_batches** | 497 / — | 514 / — | — | 536 / — |
| **columnar → iter** | 392 / — | 400 / — | 667 / 603k | 403 / 628k |
| **columnar → to_arrow** | 637 / 576k | 667 / 603k | 667 / 603k | 694 / 628k |
| **parallel → to_arrow** | 1888 / 1.70M | 2620 / 2.36M | 2620 / 2.36M | **3072 / 2.78M** |
| **auto → to_arrow** | 1857 / 1.68M | 2691 / 2.43M | 2691 / 2.43M | 2874 / 2.60M |

`stream` super-optimized: `RowParser` no longer `quick_xml::Reader<BufReader>` `lib.rs:542` (`quick-xml` 42% wall, `unescape` alloc 11%, `String` alloc 15%), now `InputBuffer` `lib.rs:546` (`auto_mmap`) + `RowSink` `lib.rs:564` (`ColumnarSink` without `TableBuilder` hash/arena) + `scan_one_row` `scanner.rs:81` (`next_row_start` `splitter.rs:107` + `parse_row` `scanner.rs:73`). Result **508 MB/s** 100 MB (was 251, +102% `459k` rows/s), 1 GB **498 MB/s** (was 234) — within 30% of columnar 651/694 (was 174% gap). `perf` streaming now `libpython` `dict` 1–2% self, not Rust — GIL floor.

`columnar → iter` is slower than `stream → iter` (400 vs 501) because it builds `TableBuilder` then iterates via `_arrow_iter` `source.py:38` (`to_batches().to_pylist()`), while `stream` yields `Cow::Borrowed` directly via `RowSink`.

## Pushdowns (100 MB, `to_arrow`)

Best-of-3, `drop_fields` / `field_mapping` / `field_types` / `dictionary` / `auto_dict` / `filter` / `schema` / `use_mmap` (`bench_extended.py:54` `PUSHDOWNS`).

| Pushdown | columnar | parallel | Notes |
|---|---|---|---|
| baseline | 681 MB/s | 2706 MB/s | 10 cols |
| `drop_half` (3 cols) | 754 (+10%) | **2996** (+10%) | `wants()` byte-jump `scanner.rs:210` saves `<Value>` walk |
| `drop_all` (11 cols) | 1160 (+66%) | **4183** (+54%) | `Finder` jump to `</Field>` without decode |
| `rename` | 567 | 2505 | `field_mapping` `plan.rs:188` one hash |
| `typed_int` | 646 | 2583 | `lexical::parse` `columnar.rs:378` |
| `typed_float` | 667 | 2548 |  |
| `dict` | 646 | 2501 | `dictionary_columns` `columnar.rs:557` |
| `auto_dict` | 569 | 1604 | forces `merge.rs:57` serial `extend`, `auto_dict_upgrade` `engine.rs:139` |
| `filter_eq` `Level==3` | 665 | 2552 | per-row `check` `plan.rs:280` then `row_dirty` |
| `filter_compare` `Field22>Field23` | 642 (45k rows) | 2346 (45k) | `compare` + `apply_compare_filter` `arrow_export.rs:29` fast path |
| `schema` ordering | 653 | 2525 | `sort_columns` `engine.rs:152` |
| `mmap` on/off | 647/656 | 2771/2590 | `auto_mmap` 2–4% single, warm-cache `rep_movs` 3% perf |

`drop_all` shows the engine is **CPU-bound**, not I/O: reducing copied fields from 10→0 gives 1.66×, yet ceiling stays ~3 GB/s.

## Chunk and memory scaling

| File | par2 | par4 | par8 | par16 | par32 | par64 |
|---|---|---|---|---|---|---|
| 10 MB | 1198 | 1956 | 2577 | 2849 | 2040 | 1573 |
| 50 MB | 1042 | 1634 | 1907 | 2193 | 2313 | 2204 |
| 100 MB | 1146 | 1801 | 2409 | 2522 | 2624 | 2775 |
| 1 GB | 1117 | 1774 | 2464 | 2489 | 2715 | 2868 |

`n ≈ min(threads=16, rows/2000)` is optimal. `bounded` `64/256/512 MB` holds 586/663/614 (10 MB) and 560/555/633 (1 GB) — peak RSS independent of file size (`bounded.rs:52` `plan_chunks`).

Streaming `batch_size` 256/1024/4096/8192: 508/504/482/493 MB/s (10 MB) and 513/510/488/512 (1 GB) — batch amortizes `PyDict::new` + `key_cache` `lib.rs:599` double hash, but `next_batch(1024)` already `allow_threads` `lib.rs:919`.

Pipeline `DropFields|FilterRows` `bench_extended.py:130`: `pipe base` 1796 MB/s 10 MB, `pipe filter` 2687 100 MB, `Pipeline Drop+Filter` 2172 10 MB / 2630 1 GB via `Pipeline::_to_arrow` `pipeline.py:42` → `plan_split` `fusion.py:130` + `collect_table` `batchpipe.py:57`.

## How to confirm I/O vs CPU bound

*Single* `read_to_columnar` 690 MB/s both `use_mmap` true/false — parser-bound. `perf` single self `field_element` 8.6%, `scan_open_tag` 8.3%, `find_raw` 8.4%, `push_field_resolved` 2.76% + `field_index.get` 1.64% — `scan_open_tag` + `memchr` dominate, not `get`. `par` self `Finder` 6.6%, `validate` 4.1%.

*Page-cache test* `tmp/test_io_bound.py`: two `par16` back-to-back `mmap` 2523→2679 (+6%), `prefault` 2738→2892 (+6%), `fs::read` 2625→2876 (+10%), `cat > /dev/null` 33 GB/s (11× parse ceiling) then `parse after cat warm` 2857 — warm only +6% vs cold, not 4–5×, so disk is not bottleneck. `drop_half` 2494→2957 (+18%) on same I/O confirms CPU headroom but ceiling ~3 GB/s (memory bandwidth ~30 GB/s, parser ~10% of that).

Run yourself:

```bash
echo 3 | sudo tee /proc/sys/vm/drop_caches
.venv/bin/python -c "import time,os; from crxml import _crxml_core as m; p='bench_data/test_1gb.xml'; s=os.path.getsize(p); t0=time.perf_counter(); m.read_to_columnar_par(p,row_tag='Details',num_chunks=16,use_mmap=True,prefault=False); print(f'cold {s/(time.perf_counter()-t0)/1e6:.1f} MB/s')"
cat bench_data/test_1gb.xml > /dev/null
# warm
.venv/bin/python benchmarks/bench_extended.py --quick --rounds 2  # or --rounds 3
```

## Engine selection guide (per goal)

| If your goal is... | Use this engine | Because... |
|---|---|---|
| Fastest `to_arrow`/`to_dataframe` | `engine="parallel"` | 2700–3000 MB/s 1 GB, `mmap`+off-GIL `rypipe/src/parallel.rs:27` |
| Minimize peak memory | `engine="auto"` + `memory="256MB"` → `bounded` | RSS = budget + `Arrow` export, not file size `bounded.rs:52` |
| Stream rows one-by-one | `engine="stream"` (now scanner-based) | 508 MB/s, lowest latency to first row, `RowSink` `lib.rs:564` no `TableBuilder` |
| Dictionary-encoded | `engine="parallel", auto_dict=True` | 1604 MB/s 1 GB, forces `merge` `merge.rs:57` |

## The ceiling

At 0.7 GB/s single / 3 GB/s parallel on a ~30 GB/s memory bus, this parser is **CPU-bound** (tokenizing + `FxHash` + `StrColumn::push` `columnar.rs:53` arena). The `memchr` scanner removed the 69% `quick-xml` slice (`docs/performance.md:59` `+27%` single, `+36%` multi2), `row_dirty:Vec<bool>` `engine.rs:16` cut `finish_row` 34%→<1%, `field_index:Vec+HashMap` cut double-probe.

**Measured ceilings (5800X, warm):**

| CPU | Single | Parallel (16) | Parallel (32) |
|---|---|---|---|
| 5800X | **714** MB/s (1 GB) / 698 (100 MB) | 2641 (1 GB) / 2720 (100 MB) | **2994** (1 GB) / 2619 (100 MB) |
| i5-1335U (est.) | 650–700 | 1.3–1.5 GB/s | — |

2 GB/s milestone cleared; 3 GB/s is the current `memchr`+`FxHash` ceiling. Next leverage is per-row `FieldId` perfect hash + unchecked `StrColumn` bump + batch `put_batch` (all benefit every adapter, see `rypipe` `writing-adapters.md`).

## Correctness & Coverage

Every number is backed by `tests/test_differential.py` vs `xml.etree.ElementTree` (ragged, empty, entities, unicode, comments with fake row tags) and `splitter` 18 tests. `benchmarks/bench_extended.py` (`all` or `--quick`) covers 104 benchmarks/file (native + source×sink + pushdowns + chunk/bounded/batch/pipeline) ×3 rounds, best-of-N after warmup.
