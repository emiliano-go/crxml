# Installation

## From PyPI

```bash
pip install crxml
```

Pre-built wheels are published for Linux x86_64, macOS (arm64 + x86_64), and
Windows x86_64. The wheel includes the compiled Rust extension so no Rust
toolchain is required.

## Verify the Rust extension

```python
from crxml._crxml_core import CrxmlReader
print("Rust backend OK")
```

## Building from source

If no pre-built wheel matches your platform, pip will build from source.
This requires the Rust toolchain.

### Prerequisites

- Python ≥ 3.10
- [Rust](https://rustup.rs) (stable)

The Rust core depends on `rypipe-core` via a **path dependency** on the
sibling [rypipe](https://github.com/emiliano-go/rypipe) repository, so
`rypipe` must be checked out next to `crxml` (i.e. `../rypipe` relative to
the `crxml` root) for `pip install .` or `maturin develop` to succeed.

### Build the Rust core

```bash
pip install maturin
maturin build --release   # builds the crate in src/crxml_core/
```

Or use `maturin develop` during development:

```bash
maturin develop --release
```

## Supported platforms

| Platform    | Wheel |
|-------------|-------|
| Linux x86_64 | ✅   |
| macOS arm64 | ✅    |
| macOS x86_64 | ✅   |
| Windows x86_64 | ✅ |
