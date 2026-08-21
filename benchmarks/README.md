# Benchmarks

Reproduce the numbers in [docs/performance.md](../docs/performance.md) on
your own hardware.

## Setup

```bash
pip install -e ".[dev]"
```

The suite generates its own synthetic files (10/50/100 MB) into
`bench_data/` on first run; the 533 MB real-export number requires a file
that is not distributed with the repo.

## Run

```bash
# Throughput + peak RSS per engine (writes results to stdout)
python benchmarks/benchmarks.py

# Phase-level profile (split scan / parse / export)
python benchmarks/bench_profile.py
```

## Methodology and caveats

- Numbers in the docs come from **one Linux machine** (specific CPU,
  kernel page-cache state, and background load all move results by
  double-digit percentages). Treat them as indicative, not contractual.
- `peak RSS` uses `resource.getrusage(RUSAGE_SELF).ru_maxrss` where
  available; it is a process-lifetime high-water mark, so run each engine
  in a fresh process for clean numbers.
- The first run includes synthetic-file generation time; discard it or
  keep the generated files and re-run.
- Thread-count sensitivity (the 4x chunk multiplier) was tuned with VTune
  on a 24-core Linux box; expect different optima elsewhere.
