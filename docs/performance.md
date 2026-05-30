# Performance

## Benchmarks

Tests on a synthetic 100 MB Crystal Reports XML file (~90K rows).

| Metric | Rust backend | lxml fallback |
|--------|-------------|---------------|
| Throughput | 155 K rows/s | 42 K rows/s |
| Parse time (100 MB) | 0.58 s | 3.8 s |
| Peak RSS | 94 MB | 182 MB |
| Memory per row (streaming) | ~34 bytes | ~1.1 KB |

Rust backend RSS stays flat from 10 MB to 100 MB inputs (~94 MB). The lxml
fallback builds a DOM tree and has higher baseline memory.

## Where time goes

- **I/O:** ~15% of parse time (kernel read syscalls)
- **XML parsing:** ~40% (quick-xml)
- **Dict construction:** ~30% (PyDict creation, key/value allocation)
- **Python overhead:** ~15% (yield, iteration boundary)

The dominant cost is allocating Python `str` objects for each field value and
constructing dicts. This overhead is inherent to the Python/Rust boundary.

## Memory breakdown

```
┌──────────────┐
│  I/O buffer  │  2 MB  (reused Vec<u8>)
│  Row dict    │  ~2 KB per row (temporary)
│  RSS floor   │  94 MB (Python interpreter + Rust allocator)
└──────────────┘
```

## Parallel mode

For files >200 MB, parallel mode distributes batches across N workers:

| Workers | 500 MB file | Speedup |
|---------|-------------|---------|
| 1       | 3.2 s       | 1.0×    |
| 2       | 1.9 s       | 1.7×    |
| 4       | 1.1 s       | 2.9×    |
| 8       | 0.9 s       | 3.6×    |

Diminishing returns past 4 workers due to IPC overhead.

## Bottleneck analysis

- **Disk I/O:** Not the bottleneck for files <1 GB on modern NVMe SSDs
- **Single-threaded Python:** Dict allocation is the bottleneck
- **lxml DOM:** Memory is the bottleneck for files >50 MB
- **Rust allocator jemalloc:** Gives better RSS stability than system malloc

## Recommendations

| File size | Strategy |
|-----------|----------|
| < 50 MB   | Sequential (no overhead) |
| 50–200 MB | Sequential or parallel(2) |
| > 200 MB  | parallel(4) |
| > 1 GB    | parallel(N_CPU), chunksize=20000 |
