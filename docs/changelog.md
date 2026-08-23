# Changelog

## 1.2.0 (2026-08-23)

### Refactor

- Extracted the columnar engine into the sibling `rypipe` workspace
  (`rypipe-core`, `rypipe-xml`, `rypipe-python`).
- `rypipe-core` is now consumed from crates.io as a versioned dependency
  (`version = "0.1"`, `mmap` feature) instead of a path dependency: building
  crxml no longer requires a sibling rypipe checkout.
- Embedded the Crystal Reports XML adapter (previously the separate
  `rypipe-xml` crate) directly in `crxml_core`.
- Renamed the internal plan type from `BuildPlan` to `rypipe_core::ExecutionPlan`.
- `Compare` filters now use `arrow::compute` kernels instead of
  `pyarrow.compute`.

### Removed

- Deleted `src/crxml_core/src/columnar.rs` and `src/crxml_core/src/splitter.rs`;
  their logic lives in rypipe now.

### Kept

- The streaming `CrxmlReader` remains in `crxml_core`.

### Packaging

- sdist now ships `LICENSE` explicitly (PEP 639 license expression) so PyPI
  accepts the upload.
- CI installs `rypipe` from PyPI for integration tests instead of cloning a
  sibling checkout.

### Testing

- All existing tests pass.

## 1.0.0 (2026-07-06)

### Bug Fixes

- **auto_dict plan lost in parallel merge**: `ColumnarEngine::new()` defaulted to `auto_dict: false`, making `auto_dict_upgrade()` a no-op. Fixed by using `ColumnarEngine::with_plan(est, plan)` to carry the build plan forward.

- **Text field parsing in bounded path**: The parser was capturing whitespace-only text nodes as field values instead of looking for `<TextValue>` children. Fixed to correctly consume `TextValue` inner text.

- **Stream engine column discovery**: Engine used first-row columns as schema; sparse columns appearing only in later rows caused crashes. Schema is now discovered across all rows.

- **Publishing workflow missing `columnar` feature**: `maturin build --features mmap` risked overriding pyproject.toml's feature list and silently dropping `columnar` from the published wheel. CI now builds from pyproject.toml defaults.

### Features

- **`prefault` parameter**: All engines accept `prefault: bool`. `True` = `MADV_WILLNEED` (speed), `False` = `MADV_SEQUENTIAL` (lower RSS). Defaults: True for columnar/parallel, False for bounded.

- **Parallel engine profiling**: `get_par_profile()` returns nanosecond timing for split-scan, off-GIL parse, and on-GIL assembly phases (gated behind `profile` Cargo feature).

- **Bounded mode RSS rewrite**: Mmap used only for initial split scan, then dropped. Chunks read via `File::seek`/`read_exact`. Peak RSS tracks the `memory=` budget, not the file size.

- **`sort_columns()` on engine**: Ensures all batch engines produce identical column order for schema-match fast path in `concat_tables()`.

### Performance

- **Splitter SIMD optimization**: `next_row_start()` searches for `<tag` in one `memmem::find` pass instead of `memchr(b'<')` (24M iterations to 465k matches). Split phase 40% faster. Total parallel throughput improved 22% (327 to 472 MB/s on 533 MB real file).

- **`find_special_regions()` single-pass**: Scans once for `b"<!"` prefix instead of two separate scans for `<!--` and `<![CDATA[`.

- **`concat_tables()` schema fast path**: Skips `promote_options='default'` when schemas already match.

### Chores

- Build features (`columnar`, `mmap`) now enabled by default in pyproject.toml. `profile` remains opt-in.
- Removed `--features mmap` from CI publishing workflow.
- Added `docs/performance.md` with environment block, per-engine speed tables, memory decomposition, and throughput ceiling.
- Correctness harness validates all engines against stream oracle across 29 test cases plus 465k-row real-file cross-check.
- README restyled to match seoslug format: quick start first, narrative "why" section, comparison table, features table, framework support, `---` section rulers.

## 0.1.0 (2026-06-01)
