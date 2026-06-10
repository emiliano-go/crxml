# Performance

## Benchmarks

| Test | Size | Rows | Time | Rows/s | MB/s | RSS |
|------|------|------|------|--------|------|-----|
| Stream | 10 MB | 9,010 | 0.043s | 211 K | 234 | 22 MB |
| Stream | 50 MB | 45,328 | 0.223s | 203 K | 224 | 45 MB |
| Stream | 100 MB | 90,384 | 0.418s | 216 K | 239 | 75 MB |
| To list | 10 MB | 9,010 | 0.052s | 174 K | 192 | 32 MB |
| To list | 50 MB | 45,328 | 0.249s | 182 K | 201 | 98 MB |
| To list | 100 MB | 90,384 | 0.478s | 189 K | 209 | 181 MB |
| Pipeline | 10 MB | 9,010 | 0.060s | 150 K | 166 | 32 MB |
| Pipeline | 50 MB | 45,328 | 0.295s | 154 K | 169 | 96 MB |
| Pipeline | 100 MB | 90,384 | 0.579s | 156 K | 173 | 176 MB |
| DataFrame | 10 MB | 9,010 | 0.320s | 28 K | 31 | 86 MB |
| DataFrame | 50 MB | 45,328 | 0.538s | 84 K | 93 | 152 MB |
| DataFrame | 100 MB | 90,384 | 0.829s | 109 K | 121 | 234 MB |

pandas is imported lazily — RSS and time for DataFrame mode includes pandas overhead.

## Where time goes

- **I/O:** ~15% of parse time (kernel read syscalls)
- **XML parsing:** ~40% (quick-xml)
- **Dict construction:** ~30% (PyDict creation, key/value allocation)
- **Python overhead:** ~15% (yield, iteration boundary)

The dominant cost is allocating Python `str` objects for each field value and
constructing dicts. This overhead is inherent to the Python/Rust boundary.

## Memory breakdown

```
┌──────────────────┐
│  I/O buffer      │  2 MB  (reused Vec<u8>)
│  Row dict        │  ~2 KB per row (temporary)
│  RSS floor       │  15 MB (Python interpreter, no pandas)
│  Pipeline stages │  ~20 MB (stage objects, fusion)
└──────────────────┘
```

RSS scales with file content (22 MB for 10 MB, 75 MB for 100 MB).

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
- **XML parsing:** quick-xml is fast; the bottleneck is Python dict construction
- **Rust allocator:** Uses system allocator with stable RSS

## Recommendations

| File size | Strategy |
|-----------|----------|
| < 50 MB   | Sequential (no overhead) |
| 50–200 MB | Sequential or parallel(2) |
| > 200 MB  | parallel(4) |
| > 1 GB    | parallel(N_CPU), chunksize=20000 |
