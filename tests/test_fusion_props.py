"""Property tests for the fusion planner.

For randomized stage chains, ``fused_iter`` (whatever mix of Rust
pushdown, Arrow compilation, and dict fallback it picks) must produce
exactly the same rows as sequentially applying each stage as a plain
stream transform.
"""
import random

import pytest

from crxml import Pipeline, RenameFields, DropFields, FilterRows
from crxml.fusion import fused_iter

NAMES = ["a", "b", "c", "d", "e"]


def _make_rows(rng: random.Random, n: int) -> list[dict]:
    rows = []
    for i in range(n):
        row = {}
        for name in NAMES:
            if rng.random() < 0.75:
                row[name] = rng.choice(["1", "2", "3", "x", "", "10"])
        if not row:
            row["a"] = "0"
        rows.append(row)
    return rows


def _random_stage(rng: random.Random):
    kind = rng.choice(["rename", "drop", "filter_eq", "filter_ne", "lambda"])
    if kind == "rename":
        src = rng.sample(NAMES, 2)
        return RenameFields({src[0]: src[0] + "_r"})
    if kind == "drop":
        return DropFields([rng.choice(NAMES)])
    if kind == "filter_eq":
        return FilterRows(field=rng.choice(NAMES), op="==", value="2")
    if kind == "filter_ne":
        return FilterRows(field=rng.choice(NAMES), op="!=", value="x")
    field = rng.choice(NAMES)

    def keep_positive(stream):
        for r in stream:
            v = r.get(field)
            if v is not None and v.isdigit():
                yield r

    return keep_positive


def _sequential(rows: list[dict], stages: list) -> list[dict]:
    out = rows
    for st in stages:
        out = st(out)
    return list(out)


@pytest.mark.parametrize("seed", range(40))
def test_fused_iter_matches_sequential(seed):
    rng = random.Random(seed)
    rows = _make_rows(rng, n=25)
    stages = [_random_stage(rng) for _ in range(rng.randint(1, 6))]
    expected = _sequential(rows, stages)
    got = list(fused_iter(iter(rows), stages))
    assert got == expected


@pytest.mark.parametrize("seed", range(10))
def test_pipeline_iteration_matches_sequential(seed):
    rng = random.Random(seed + 1000)
    rows = _make_rows(rng, n=25)
    stages = [_random_stage(rng) for _ in range(rng.randint(1, 5))]
    expected = _sequential(rows, stages)
    pipe = Pipeline(iter(rows))
    for st in stages:
        pipe = pipe | st
    assert list(pipe) == expected


def test_empty_stream_random_chains():
    for seed in range(10):
        rng = random.Random(seed + 2000)
        stages = [_random_stage(rng) for _ in range(3)]
        assert list(fused_iter(iter([]), stages)) == []
