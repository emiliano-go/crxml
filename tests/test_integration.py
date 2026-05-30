"""Integration tests spanning source → stages → sinks with benchmark files."""
import pytest
from crxml import CrystalXMLSource, RenameFields, CastTypes, DropFields, FilterRows, to_dataframe, to_csv, collect


class TestEndToEnd10MB:
    """Smoke tests against the 10 MB benchmark file (9010 rows)."""

    def test_parse_all_rows(self, bench_10mb):
        rows = collect(CrystalXMLSource(bench_10mb, row_tag="Details"))
        assert len(rows) == 9010

    def test_schema_fields(self, bench_10mb):
        fields = CrystalXMLSource(bench_10mb, row_tag="Details").schema()
        assert len(fields) >= 8  # CR XML has multiple fields per row

    def test_pipeline_rename(self, bench_10mb):
        first_key = None
        for r in CrystalXMLSource(bench_10mb, row_tag="Details"):
            first_key = list(r.keys())[0]
            break
        rename_map = {first_key: "renamed"}
        rows = collect(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | RenameFields(rename_map)
        )
        assert "renamed" in rows[0]
        assert first_key not in rows[0]

    def test_pipeline_drop_fields(self, bench_10mb):
        first_key = None
        for r in CrystalXMLSource(bench_10mb, row_tag="Details"):
            first_key = list(r.keys())[0]
            break
        rows = collect(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | DropFields([first_key])
        )
        assert first_key not in rows[0]

    def test_pipeline_cast(self, bench_10mb):
        # Find a numeric field – synthetic file may not have one
        # Just test that CastTypes with no-op mapping works
        rows = collect(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | CastTypes({})
        )
        assert len(rows) == 9010

    def test_pipeline_filter(self, bench_10mb):
        first_key = None
        for r in CrystalXMLSource(bench_10mb, row_tag="Details"):
            first_key = list(r.keys())[0]
            break
        rows = collect(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | FilterRows(lambda r: r.get(first_key) == r.get(first_key))
        )
        assert len(rows) == 9010

    def test_pipeline_all_stages(self, bench_10mb):
        first_key = None
        for r in CrystalXMLSource(bench_10mb, row_tag="Details"):
            first_key = list(r.keys())[0]
            break
        rows = collect(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | RenameFields({first_key: "renamed"})
            | DropFields(["Level"])
            | CastTypes({})
            | FilterRows(lambda r: True)
        )
        assert len(rows) == 9010
        assert "renamed" in rows[0]

    def test_to_dataframe(self, bench_10mb):
        df = to_dataframe(CrystalXMLSource(bench_10mb, row_tag="Details"))
        assert df.shape == (9010, 11)

    def test_to_dataframe_with_stages(self, bench_10mb):
        df = to_dataframe(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | DropFields(["Level", "Section"])
        )
        assert df.shape[1] == 9  # dropped 2 of 11 fields

    def test_csv_roundtrip(self, bench_10mb, tmp_path):
        out = tmp_path / "out.csv"
        to_csv(CrystalXMLSource(bench_10mb, row_tag="Details"), out)
        assert out.exists()
        assert out.stat().st_size > 0

    def test_parallel_mode(self, bench_10mb):
        def keep_all(r):
            return True
        rows = collect(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | FilterRows(keep_all)
            | CastTypes({})
            | DropFields([])
        )
        assert len(rows) == 9010


@pytest.mark.bench
class TestEndToEnd50MB:
    def test_parse_all_rows(self, bench_50mb):
        rows = collect(CrystalXMLSource(bench_50mb, row_tag="Details"))
        assert len(rows) == 45328

    def test_to_dataframe(self, bench_50mb):
        df = to_dataframe(CrystalXMLSource(bench_50mb, row_tag="Details"))
        assert df.shape[0] == 45328


@pytest.mark.bench
class TestEndToEnd100MB:
    def test_parse_all_rows(self, bench_100mb):
        rows = collect(CrystalXMLSource(bench_100mb, row_tag="Details"))
        assert len(rows) == 90384

    def test_to_dataframe(self, bench_100mb):
        df = to_dataframe(CrystalXMLSource(bench_100mb, row_tag="Details"))
        assert df.shape[0] == 90384


class TestCustomStages:
    """Test that custom stages compose correctly."""

    def test_generator_stage(self, bench_10mb):
        def upper_names(stream):
            for r in stream:
                if "Section" in r:
                    r["Section"] = r["Section"].upper()
                yield r
        rows = list(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | upper_names
        )
        assert isinstance(rows[0]["Section"], str)

    def test_map_stage(self, bench_10mb):
        def add_key(stream):
            return map(lambda r: {**r, "_tagged": True}, stream)
        rows = list(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | add_key
        )
        assert rows[0].get("_tagged") is True

    def test_fusable_class_stage(self, bench_10mb):
        class StripWhitespace:
            def apply(self, record):
                return {k: v.strip() if isinstance(v, str) else v for k, v in record.items()}
            def __call__(self, stream):
                return map(self.apply, stream)
        rows = list(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | StripWhitespace()
        )
        first_key = list(rows[0].keys())[0]
        assert isinstance(rows[0][first_key], str)
