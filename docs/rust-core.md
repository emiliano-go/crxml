# Rust Core

The native accelerator is a PyO3 crate at `src/crxml_core/`.

## Crate structure

```
src/crxml_core/
├── Cargo.toml
└── src/
    ├── lib.rs          # CrxmlReader class + thin columnar FFI wrappers
    └── xml/            # Crystal Reports XML adapter for rypipe-core
        ├── decoder.rs  # CrystalXmlDecoder implements RecordParser
        ├── splitter.rs # CrystalXmlSplitter implements Splitter
        ├── error.rs    # adapter error type
        └── mod.rs
```

### `lib.rs`: streaming engine and columnar wrappers

`lib.rs` now contains two parts:

1. **`CrxmlReader`**: the streaming XML parser (remains in crxml).
2. **Thin columnar FFI wrappers**: `#[pyfunction]` entry points (`read_to_columnar`,
   `read_to_columnar_multi`, `read_to_columnar_par`, `read_to_columnar_bounded`) that
   build an `rypipe_core::ExecutionPlan` and delegate parsing to `rypipe-core` through
   the embedded Crystal Reports XML adapter in `src/xml/`.

#### `CrxmlReader`

A single Python-exposed class:

```rust
#[pyclass]
struct CrxmlReader {
    source: PathBuf,
    row_tag: String,
    buf: Vec<u8>,
    inner_buf: Vec<u8>,
}
```

- `__iter__`, returns `self`
- `__next__`, reads the next row as a `PyDict`

The reader walks the XML stream, finds `<row_tag>` elements, and extracts
field key/value pairs from nested `<Field>` and `<Text>` elements.

## Dependencies

| Crate        | Purpose                        |
|--------------|--------------------------------|
| `pyo3`       | Python bindings                |
| `quick-xml`  | Streaming XML reader           |
| `arrow`      | Arrow C Data Interface export  |
| `mimalloc`   | Fast allocator (replaces system malloc, ~27% CPU savings) |
| `rypipe-core`| Generic columnar/parallel/bounded engine (from crates.io) |
| `memchr`     | Fast substring scans for the XML splitter |
| `simdutf8`   | SIMD UTF-8 validation for the XML decoder |
| `thiserror`  | Adapter error derives            |

The `rypipe-core` crate is a versioned dependency resolved from crates.io
(see `src/crxml_core/Cargo.toml`):

```toml
rypipe-core = { version = "0.1", features = ["mmap"] }
```

No sibling checkout is required to build crxml. To hack on the engine itself,
clone [rypipe](https://github.com/emiliano-go/rypipe) separately and point the
dependency at your checkout with a cargo `[patch]` entry.

## Building

```bash
# Development build (editable)
maturin develop --release

# Production wheel
maturin build --release
```

The `pyproject.toml` `[tool.maturin]` section controls the build:

```toml
[tool.maturin]
module-name = "crxml._crxml_core"
manifest-path = "src/crxml_core/Cargo.toml"
```

## Code style

- Rust 2021 edition
- `cargo fmt` for formatting
- `cargo clippy`, no warnings allowed
- Unsafe code is denied by default (`#![deny(unsafe_code)]`)

## Testing

```bash
# Rust unit tests (streaming engine + XML adapter)
PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 cargo test --manifest-path src/crxml_core/Cargo.toml --all-features

# Python test suite
pytest
```
