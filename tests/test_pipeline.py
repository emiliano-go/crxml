"""Tests for Pipeline composition, chaining, and lazy evaluation."""
import pytest
from crxml import CrystalXMLSource, Pipeline, RenameFields, CastTypes, DropFields, FilterRows, collect
from crxml.pipeline import Pipeline as PipelineCls


class TestPipelineConstruction:
    def test_source_or_stage_returns_pipeline(self, bench_10mb):
        p = CrystalXMLSource(bench_10mb, row_tag="Details") | RenameFields({"a": "b"})
        assert isinstance(p, Pipeline)

    def test_pipeline_or_stage_returns_new_pipeline(self, bench_10mb):
        p1 = CrystalXMLSource(bench_10mb, row_tag="Details") | RenameFields({"a": "b"})
        p2 = p1 | CastTypes({})
        assert isinstance(p2, Pipeline)
        assert p2 is not p1  # immutability

    def test_pipeline_immutable_stages(self, bench_10mb):
        p1 = CrystalXMLSource(bench_10mb, row_tag="Details") | RenameFields({"a": "b"})
        p2 = p1 | CastTypes({})
        # p1 should NOT have the CastTypes stage
        assert len(p1._stages) == 1
        assert len(p2._stages) == 2

    def test_chaining_three_stages(self, bench_10mb):
        p = (
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | RenameFields({"a": "b"})
            | CastTypes({})
            | DropFields(["x"])
        )
        assert len(p._stages) == 3


class TestPipelineIteration:
    def test_iterate_yields_dicts(self, bench_10mb):
        p = CrystalXMLSource(bench_10mb, row_tag="Details")
        for i, row in enumerate(p):
            assert isinstance(row, dict)
            if i >= 5:
                break

    def test_pipeline_with_stages(self, bench_10mb):
        first_key = None
        for row in CrystalXMLSource(bench_10mb, row_tag="Details"):
            first_key = list(row.keys())[0]
            break
        assert first_key is not None

        p = (
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | DropFields([first_key])
        )
        for row in p:
            assert first_key not in row
            break

    def test_pipeline_lazy(self, bench_10mb):
        """Pipeline should not iterate until __iter__ is called."""
        called = False
        class TrackingSource:
            def __iter__(self):
                nonlocal called
                called = True
                return iter([])
        p = Pipeline(TrackingSource()) | RenameFields({"a": "b"})
        assert not called
        list(p)
        assert called


class TestPipelineEdgeCases:
    def test_empty_source(self):
        p = Pipeline(iter([]))
        result = list(p)
        assert result == []

    def test_empty_source_with_stages(self):
        p = Pipeline(iter([])) | RenameFields({"a": "b"})
        result = list(p)
        assert result == []

    def test_no_stages(self, bench_10mb):
        rows = list(CrystalXMLSource(bench_10mb, row_tag="Details"))
        assert len(rows) > 0

    def test_double_iteration(self, bench_10mb):
        """Each iteration should re-read the file."""
        src = CrystalXMLSource(bench_10mb, row_tag="Details")
        rows1 = list(src)
        rows2 = list(src)
        assert len(rows1) == len(rows2)

    def test_parallel_requires_picklable(self, bench_10mb):
        p = (
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | FilterRows(lambda r: True)
        )
        pp = p.parallel(workers=2)
        with pytest.raises(TypeError, match="not picklable"):
            list(pp)

    def test_parallel_with_picklable_stages(self, bench_10mb):
        def keep_all(r):
            return True
        p = (
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | FilterRows(keep_all)
        )
        # Should not raise
        pp = p.parallel(workers=2)
        assert pp._workers == 2
