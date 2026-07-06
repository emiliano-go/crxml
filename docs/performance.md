# Performance

## Environment

All measurements recorded on a single development machine:

| Component | Detail |
|---|---|
| **CPU** | 13th Gen Intel Core i5-1335U (10 cores: 2 P + 8 E, 12 threads) |
| **L1d** | 352 KiB (10 instances) |
| **L2** | 6.5 MiB (4 instances) |
| **L3** | 12 MiB (1 instance) |
| **RAM** | 15 GiB LPDDR5 (system-unknown speed) |
| **OS** | Arch Linux, kernel 7.0.9-arch2-1 |
| **Python** | 3.14.5 |
| **pyarrow** | 24.0.0 |
| **crxml** | 0.3.0 |
| **Git SHA** | `bbc8a172` |
| **Build** | release, LTO enabled, mimalloc allocator, features `columnar` + `mmap` + `profile` |

All runs are **warm-cache** (one warmup parse, collection, then measured).  Each number is the best of 3 runs after variance stabilized.  Parallel-path variance was ~8% on the 533 MB file, stream-path variance ~15%.

## Input files

| File | Size | Rows | Fields/row | Origin |
|---|---|---|---|---|
| `test_10mb.xml` | 10 MB | 9,010 | 10 | Synthetic (`benchmarks/benchmarks.py`) |
| `test_50mb.xml` | 50 MB | 45,328 | 10 | Synthetic |
| `test_100mb.xml` | 100 MB | 90,384 | 10 | Synthetic |
| `test_533mb.xml` | 533 MB | 465,136 | 11 | Real Crystal Reports export |

Synthetic files use uniform rows, every field present on every row, and low cardinality
(most fields repeat).  These properties flatter parallel load balance and dictionary
encoding, so synthetic numbers are **directional only**.  The 533 MB real export is the
ground truth for all reported conclusions.

Synthetic files have 10 columns (including `FieldG`, absent from the real export) with
cardinalities ranging from 1 (`Level`, `Section`, `Text20`) through 15 (`Field38`,
`Field39`) to near-unique (`Field22`: 8,965 / 9,010).

### Field cardinality (real 533 MB file, 465,136 rows)

Only 5 of 11 columns have high cardinality (≥1,000 distinct values);
the other 6 are dictionary-encoding candidates.  Not every column appears in every row
(`Field72` and `Text21` are sparse); the columnar engine discovers all distinct column
names across all rows.

| Column | Distinct values |
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

## Speed — end-to-end `to_dataframe()` (the user's actual goal)

| Engine | 10 MB | 50 MB | 100 MB | 533 MB |
|---|---|---|---|---|
| **Stream** | 248 ms / 40 MB/s / 36k r/s | 1.35 s / 36 MB/s / 34k r/s | 2.27 s / 44 MB/s / 40k r/s | **12.8 s / 42 MB/s / 36k r/s** |
| **Parallel** (8 workers) | 30 ms / 335 MB/s / 300k r/s | 124 ms / 402 MB/s / 365k r/s | 213 ms / 469 MB/s / 424k r/s | **1.13 s / 472 MB/s / 412k r/s** |
| **Parallel + auto-dict** | 38 ms | 172 ms | 312 ms | **2.0 s / 267 MB/s** |

Key observations:

- **Parallel throughput improves with file size** (335 → 472 MB/s) because split-scan and
  worker startup are fixed costs that amortize.  The 533 MB number (472 MB/s) is the
  asymptotic rate.
- **Parallel is ~11× faster than stream** on the 533 MB real file.
- **Auto-dict adds ~0.9 s of on-GIL overhead** at 533 MB (dictionary encoding happens
  after the GIL is reacquired).  Use it only when downstream readers benefit from
  dictionary-encoded Arrow columns.

## Parallel-path breakdown (533 MB real file)

| Phase | Time | % of wall | Notes |
|---|---|---|---|
| Split-scan (serial) | 257 ms | 23% | Two SIMD scans for `<tag` + special regions |
| Off-GIL parse (N=8) | 781 ms | 69% | quick-xml event loop, unescape, field copy |
| On-GIL assembly | 25 ms | 2% | Arrow table construction, GIL-held |
| Profile coverage | — | 94% | Remaining 6% = Python overhead, GC, import |

This breakdown is the honest map of optimization headroom:

- **Parse (69%) is the ceiling.**  The parser tokenizes every XML element even when
  the `BuildPlan` drops the field.  Skipping unwanted-field bytes is the remaining
  high-leverage improvement.
- **Split-scan (23%) is nearly free** after the `<tag` SIMD change.  Further wins here
  are single-digit percentages.
- **GIL assembly (2%) is a dead lever.**  Collapsing it further yields nothing.

## Engine selection guide (per goal)

| If your goal is... | Use this engine | Because... |
|---|---|---|
| Fastest `to_dataframe()` | `engine="parallel"` | ~11× faster than stream; mmap + off-GIL parse |
| Minimize peak memory | `engine="bounded"` | RSS tracks the `memory=` budget, not file size |
| Stream rows one-by-one | Stream (default `CrxmlReader`) | Lowest latency to first row; no columnar overhead |
| Dictionary-encoded columns | `engine="parallel"` with `auto_dict=True` | Encodes low-cardinality columns; ~0.9 s GIL tax |

## Memory

### Parallel (mmap) — 533 MB file

| Metric | Value |
|---|---|
| Peak RSS | 534 MB |
| File in page cache | ~533 MB (mmap) |
| Workload buffers | ~21 MB (columnar scratch + Arrow) |
| Total allocations | 7,725 across 465,136 rows |
| Largest single allocation | 533 MB (mmap) |

The mmap path maps the file into virtual address space and pages it in on demand.
The columnar engine's output buffers are the only additional allocation of consequence.
7,725 allocations for 465k rows (~60 allocations/row) is extremely allocation-efficient.

### Stream (BufReader) — 533 MB file

| Metric | Value |
|---|---|
| Peak RSS | ~1.07 GB |
| Explanation | BufReader (128 KiB) + accumulating Python dicts (one per row) |

The stream reader holds every parsed row as a Python dict until the consumer drains the
iterator.  At 465k rows × ~11 columns, the dict overhead dominates.  Use the stream reader
only when processing rows incrementally (e.g., writing to a file) rather than collecting
them all.

## Bounded-engine memory curve

The bounded engine (`engine="bounded"`, `memory=`) controls peak RSS by splitting the
file into chunks that fit within the budget.  Each chunk is parsed, converted to Arrow,
and concatenated.  Concatenation cost grows as the number of chunks increases.

*[TODO: insert table of wall time vs RSS vs memory= budget]*

The bounded engine's key property: **peak RSS is independent of file size** once the
file exceeds the budget.  A 10 GB file with `memory="500MB"` peaks at ~500 MB + fixed
overhead, not 10 GB.

## mmap vs fs::read

| Aspect | mmap | fs::read |
|---|---|---|
| Time (warm cache) | Identical | Identical |
| RSS (file ≤ RAM) | File appears in page cache | File copied into heap |
| RSS (file > RAM) | Pages evicted under pressure | Heap holds the full copy |
| Cold-cache startup | Page-fault-driven (first touch) | Sequential read-ahead |

On warm cache the RSS delta is near zero because the kernel's page cache already holds
the file.  The case for mmap is files near or exceeding physical RAM, where the OS can
evict pages under memory pressure.  The case for `fs::read` is cold-cache streaming,
where the kernel's read-ahead is more predictable than page faults.

## Correctness

Every performance measurement in this document is backed by a correctness cross-check:
all three engines (stream, columnar, parallel) produce byte-identical field values
against both the stream-oracle and against `xml.etree.ElementTree` on the synthetic
corpus.  The parallel engine's row-split boundaries are validated by the splitter test
suite (18 tests covering prefix collision, CDATA/comment skipping, fallback, and random
input).  No engine cuts corners.

## The ceiling

At 472 MB/s on a machine with ~30 GB/s of memory bandwidth, this parser is
**CPU-bound**, not bandwidth-bound.  The bottleneck is not moving bytes — it's
tokenizing XML elements, unescaping entities, and copying field values.

The breakdown says parse is 69% of wall time and the biggest sub-cost is the
quick-xml event loop (tokenizing every `<Field>`, `<Text>`, `<FormattedValue>`,
`<Value>`, `<TextValue>` child even when the field is dropped by the `BuildPlan`).
The remaining high-leverage improvement is **skip-bytes-for-unwanted-fields**:
detecting a dropped column name and memmem-skipping to `</Field>` or `</Text>`
without tokenizing children.

**Honest throughput ceiling for this codebase:**

| CPU | Estimated ceiling (parallel) |
|---|---|
| i5-1335U (this machine) | ~500–550 MB/s |
| Ryzen 7 5800X (desktop) | ~800 MB/s – 1.1 GB/s |

2 GB/s would require a genuinely different parse strategy (a hand-rolled scanner
for the fixed Crystal Reports XML structure that skips quick-xml's generality),
which is a separate project, not a tuning pass.
