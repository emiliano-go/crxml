# Rust Core

The native accelerator is a PyO3 crate at `src/crxml_core/`.

## Crate structure

```
src/crxml_core/
├── Cargo.toml
└── src/
    └── lib.rs          # CrxmlReader class
```

### `CrxmlReader`

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

| Crate       | Purpose                        |
|-------------|--------------------------------|
| `pyo3`      | Python bindings                |
| `quick-xml` | Streaming XML reader           |
| `memchr`    | Fast byte searching (used internally by quick-xml) |

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
- Unsafe code is prohibited (`#![deny(unsafe_code)]`)

## Testing

```bash
# Rust unit tests
cargo test --manifest-path src/crxml_core/Cargo.toml

# Integration via Python
python -c "from crxml._crxml_core import CrxmlReader; r = CrxmlReader('test.xml', 'Details'); print(next(r))"
```

## Security

- **No unsafe code**, the entire crate is safe Rust
- **Input validation**, XML is assumed trusted (users control their source
  files). Buffer sizes are managed by quick-xml.
- **Buffer limits**, individual field values are bounded by the XML entity
  size. No unbounded allocations.
