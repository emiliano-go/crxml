"""Tests for CrystalXMLSource parsing from file paths."""
import pytest
from pathlib import Path
from crxml import CrystalXMLSource


class TestSourceInit:
    def test_source_with_str_path(self, bench_10mb):
        src = CrystalXMLSource(str(bench_10mb), row_tag="Details")
        row = next(iter(src))
        assert isinstance(row, dict)
        assert len(row) > 0

    def test_source_with_path_object(self, bench_10mb):
        src = CrystalXMLSource(bench_10mb, row_tag="Details")
        row = next(iter(src))
        assert isinstance(row, dict)

    def test_source_file_not_found(self):
        with pytest.raises(FileNotFoundError, match="not found"):
            CrystalXMLSource("/nonexistent/path.xml")

    def test_filter_validated_eagerly(self, bench_10mb):
        """Bad filter specs must fail at construction, not mid-parse."""
        bad_specs = [
            {"op": "==", "value": "x"},                      # no field
            {"field": "Level", "value": "x"},                # no op
            {"field": "Level", "op": "~=", "value": "x"},    # bad constant op
            {"field_a": "a", "op": ">", "value": "1"},       # missing field_b
            {"field_a": "a", "op": "~", "field_b": "b"},     # bad compare op
            "Level == 1",                                    # not a dict
        ]
        for spec in bad_specs:
            with pytest.raises(ValueError):
                CrystalXMLSource(bench_10mb, row_tag="Details", filter=spec)

    def test_filter_valid_specs_accepted(self, bench_10mb):
        CrystalXMLSource(
            bench_10mb, row_tag="Details",
            filter={"field": "Level", "op": "==", "value": "1"},
        )
        CrystalXMLSource(
            bench_10mb, row_tag="Details",
            filter={"field_a": "a", "op": ">=", "field_b": "b"},
        )
        CrystalXMLSource(
            bench_10mb, row_tag="Details",
            filter={"field": "Level", "op": "ne", "value": "2"},
        )

    def test_source_custom_row_tag(self, bench_10mb):
        src = CrystalXMLSource(bench_10mb, row_tag="Details")
        rows = list(src)
        assert len(rows) > 0

    def test_source_wrong_row_tag_yields_empty(self, bench_10mb):
        src = CrystalXMLSource(bench_10mb, row_tag="NonExistentTag")
        rows = list(src)
        assert len(rows) == 0


class TestSchema:
    def test_schema_returns_fields(self, bench_10mb):
        fields = CrystalXMLSource(bench_10mb, row_tag="Details").schema()
        assert isinstance(fields, list)
        assert len(fields) > 0
        assert all(isinstance(f, str) for f in fields)

    def test_schema_does_not_consume_stream(self, bench_10mb):
        src = CrystalXMLSource(bench_10mb, row_tag="Details")
        fields = src.schema()
        rows = list(src)
        assert len(rows) > 0
        for f in fields:
            assert f in rows[0]

    def test_schema_empty_file(self, tmp_path):
        p = tmp_path / "empty.xml"
        p.write_text("<root></root>")
        fields = CrystalXMLSource(p, row_tag="Details").schema()
        assert fields == []


class TestIteration:
    def test_iteration_yields_dicts(self, bench_10mb):
        for row in CrystalXMLSource(bench_10mb, row_tag="Details"):
            assert isinstance(row, dict)
            break

    def test_iteration_all_rows(self, bench_10mb):
        rows = list(CrystalXMLSource(bench_10mb, row_tag="Details"))
        assert len(rows) == 21047

    def test_iteration_keys_are_strings(self, bench_10mb):
        for row in CrystalXMLSource(bench_10mb, row_tag="Details"):
            for k, v in row.items():
                assert isinstance(k, str)
                assert isinstance(v, str)
            break

    def test_multiple_iterations(self, bench_10mb):
        src = CrystalXMLSource(bench_10mb, row_tag="Details")
        rows1 = list(src)
        rows2 = list(src)
        assert rows1 == rows2
