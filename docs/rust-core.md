# Rust Core

The native accelerator is a PyO3 crate at `src/crxml_core/`.

## Crate structure

```
src/crxml_core/
├── Cargo.toml
└── src/
    └── lib.rs          # CrxmlReader class + thin columnar FFI wrappers
```

### `lib.rs`: streaming engine and columnar wrappers

`lib.rs` now contains two parts:

1. **`CrxmlReader`**: the streaming XML parser (remains in crxml).
2. **Thin columnar FFI wrappers**: `#[pyfunction]` entry points (`read_to_columnar`,
   `read_to_columnar_multi`, `read_to_columnar_par`, `read_to_columnar_bounded`) that
   build an `rypipe_core::ExecutionPlan` and delegate parsing to `rypipe-core` /
   `rypipe-xml`.

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
| `rypipe-core`| Generic columnar/parallel/bounded engine (path dependency) |
| `rypipe-xml` | Crystal Reports XML decoder/splitter (path dependency) |

The two `rypipe` crates are resolved via path dependencies pointing outside
this repository (see `src/crxml_core/Cargo.toml`):

```toml
rypipe-core = { path = "../../../rypipe/crates/rypipe-core", features = ["mmap"] }
rypipe-xml  = { path = "../../../rypipe/crates/rypipe-xml" }
```

You must therefore clone [rypipe](https://github.com/emiliano-go/rypipe) as a
sibling of the crxml repository before any cargo or maturin build:

```bash
git clone https://github.com/emiliano-go/rypipe ../rypipe
```

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
# Rust unit tests (streaming engine)
cargo test --manifest-path src/crxml_core/Cargo.toml

# Columnar / parallel / bounded paths are exercised through Python/pytest
pytest
```

## Security

- **Unsafe denied by default** (`#![deny(unsafe_code)]`) in crxml itself.
  The remaining `unsafe` blocks for mmap I/O and SIMD-validated UTF-8 live in
  the `rypipe` workspace. The `_PyDict_NewPresized` private-CAPI call was
  removed after benchmarking showed only a 3.5% overall gain.
- **Input validation**, XML is assumed trusted (users control their source
  files). Buffer sizes are managed by quick-xml.
- **Buffer limits**, individual field values are bounded by the XML entity
  size. No unbounded allocations.
