#!/usr/bin/env bash
set -euo pipefail

rm -rf dist

maturin build --release --manylinux 2014 --zig -o dist
maturin sdist -o dist

twine check dist/*
twine upload dist/* --verbose
