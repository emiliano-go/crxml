"""Batch-pipeline correctness gates.

The standing invariants from the architecture plan:
- Oracle equivalence: chain output equals an independent row-by-row
  reference over the same input.
- Fusion equivalence: fused == unfused == oracle.
- Selection-vector invariant: deferred compaction equals eager filtering.
- Determinism: same output regardless of batch size.
"""

import pyarrow as pa
import pytest

from crxml import CastTypes, DropFields, FilterRows, Pipeline, RenameFields
from crxml.batchpipe import ArrowSource, build_chain, collect_table, iter_dicts

ROWS = [
    {"name": "Alice", "age": "30", "city": "NYC"},
    {"name": "Bob", "age": "25", "city": "LA"},
    {"name": "Cara", "age": "35", "city": "NYC"},
    {"name": "Dan", "age": "40", "city": None},
]


def table():
    return pa.Table.from_pylist(ROWS)


def oracle(stages, rows=ROWS):
    """Independent reference: row-at-a-time through stage.apply."""
    out = []
    for r in rows:
        rec = dict(r)
        for s in stages:
            rec = s.apply(rec)
            if rec is None:
                break
        else:
            out.append(rec)
    return out


def run_chain(stages, batch_size=2):
    op, trailing = build_chain(table(), stages, batch_size=batch_size)
    assert not trailing
    return list(iter_dicts(op))


class TestOracleEquivalence:
    def test_rename_drop(self):
        stages = [RenameFields({"name": "full_name"}), DropFields(["city"])]
        assert run_chain(stages) == oracle(stages)

    def test_filter_eq(self):
        stages = [FilterRows(field="city", op="==", value="NYC")]
        assert run_chain(stages) == oracle(stages)

    def test_filter_ne_null_semantics(self):
        # Dict path keeps a null (missing) value under '!=': verify parity.
        stages = [FilterRows(field="city", op="!=", value="NYC")]
        assert run_chain(stages) == oracle(stages)

    def test_lambda_fallback(self):
        stages = [CastTypes({"age": int}), FilterRows(lambda r: r["age"] > 28)]
        assert run_chain(stages) == oracle(
            [CastTypes({"age": int}), FilterRows(lambda r: r["age"] > 28)]
        )

    def test_mixed_fused_and_fallback(self):
        stages = [
            RenameFields({"age": "years"}),
            CastTypes({"years": int}),
            FilterRows(field="city", op="==", value="NYC"),
            DropFields(["name"]),
        ]
        assert run_chain(stages) == oracle(stages)


class TestDeterminism:
    @pytest.mark.parametrize("batch_size", [1, 2, 3, 1024])
    def test_batch_size_invariant(self, batch_size):
        stages = [
            FilterRows(field="city", op="!=", value="LA"),
            RenameFields({"name": "n"}),
        ]
        assert run_chain(stages, batch_size=batch_size) == oracle(stages)


class TestSelectionVector:
    def test_deferred_compaction_matches_eager(self):
        # filter | rename | drop: selection carried through the fused
        # segment, compacted only at the sink.
        stages = [
            FilterRows(field="city", op="==", value="NYC"),
            RenameFields({"age": "years"}),
            DropFields(["city"]),
        ]
        got = run_chain(stages)
        assert got == oracle(stages)

    def test_two_filters_intersect(self):
        stages = [
            FilterRows(field="city", op="==", value="NYC"),
            FilterRows(field="age", op="!=", value="30"),
        ]
        assert run_chain(stages) == oracle(stages)


class TestTableSink:
    def test_collect_table_matches_dicts(self):
        stages = [FilterRows(field="city", op="==", value="NYC")]
        op, trailing = build_chain(table(), stages, batch_size=2)
        assert not trailing
        tbl = collect_table(op)
        assert tbl.to_pylist() == run_chain(stages)

    def test_empty_result(self):
        stages = [FilterRows(field="city", op="==", value="Nowhere")]
        op, _ = build_chain(table(), stages, batch_size=2)
        assert collect_table(op).num_rows == 0


class TestPipelineIntegration:
    def test_pipeline_to_arrow_matches_iteration(self, tmp_path):
        xml = (
            b"<R>"
            b'<Row a="1" b="x"/><Row a="2" b="y"/><Row a="3" b="x"/>'
            b"</R>"
        )
        p = tmp_path / "t.xml"
        p.write_bytes(xml)
        from crxml import CrystalXMLSource

        pipe = (
            Pipeline(CrystalXMLSource(p, row_tag="Row"))
            | FilterRows(field="b", op="==", value="x")
            | RenameFields({"a": "id"})
        )
        rows_iter = list(pipe)
        tbl = pipe._to_arrow()
        assert tbl is not None
        assert tbl.to_pylist() == rows_iter
