# Changelog

## 0.1.0 (2026-06-01)

Initial release.

- Rust parser via PyO3 (quick-xml streaming reader)
- Pipeline API with `|` composition
- Four built-in stages: RenameFields, CastTypes, DropFields, FilterRows
- Parallel execution mode (ProcessPoolExecutor)

- Sinks: to_dataframe, to_csv, collect
- Schema inspection via `.schema()`
- CR XML auto-detection (attribute vs element style)
