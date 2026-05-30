"""Fixtures shared across test modules."""
from pathlib import Path
import pytest


BENCH_DIR = Path(__file__).resolve().parent.parent / "bench_data"


def pytest_addoption(parser):
    parser.addoption(
        "--run-bench", action="store_true", default=False,
        help="Run tests that use large benchmark files",
    )


def pytest_collection_modifyitems(config, items):
    if not config.getoption("--run-bench"):
        skip_bench = pytest.mark.skip(reason="use --run-bench to enable")
        for item in items:
            if "bench" in item.keywords:
                item.add_marker(skip_bench)


@pytest.fixture
def bench_10mb() -> Path:
    p = BENCH_DIR / "test_10mb.xml"
    assert p.exists(), f"Missing benchmark file: {p}"
    return p


@pytest.fixture
def bench_50mb() -> Path:
    p = BENCH_DIR / "test_50mb.xml"
    assert p.exists(), f"Missing benchmark file: {p}"
    return p


@pytest.fixture
def bench_100mb() -> Path:
    p = BENCH_DIR / "test_100mb.xml"
    assert p.exists(), f"Missing benchmark file: {p}"
    return p


@pytest.fixture
def sample_rows() -> list[dict]:
    """Synthetic rows that mimic CR XML output, no file I/O needed."""
    return [
        {"name": "Alice", "age": "30", "city": "NYC"},
        {"name": "Bob", "age": "25", "city": "LA"},
        {"name": "Carol", "age": "35", "city": "CHI"},
    ]
