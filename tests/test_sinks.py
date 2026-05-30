"""Tests for sinks: to_dataframe, to_csv, collect."""
import csv
import tempfile
from pathlib import Path
import pytest
from crxml import CrystalXMLSource, Pipeline, RenameFields, CastTypes, DropFields, FilterRows, to_dataframe, to_csv, collect


class TestCollect:
    def test_collect_basic(self, bench_10mb):
        rows = collect(CrystalXMLSource(bench_10mb, row_tag="Details"))
        assert isinstance(rows, list)
        assert len(rows) > 0
        assert isinstance(rows[0], dict)

    def test_collect_with_stages(self, bench_10mb):
        rows = collect(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | RenameFields({"level": "Level"})
        )
        assert len(rows) > 0

    def test_collect_empty(self):
        rows = collect(Pipeline(iter([])))
        assert rows == []


class TestToDataFrame:
    def test_dataframe_basic(self, bench_10mb):
        df = to_dataframe(CrystalXMLSource(bench_10mb, row_tag="Details"))
        assert df.shape[0] > 0
        assert df.shape[1] > 0

    def test_dataframe_chunksize(self, bench_10mb):
        df = to_dataframe(
            CrystalXMLSource(bench_10mb, row_tag="Details"),
            chunksize=500,
        )
        assert df.shape[0] > 0

    def test_dataframe_with_stages(self, bench_10mb):
        first_key = None
        for r in CrystalXMLSource(bench_10mb, row_tag="Details"):
            first_key = list(r.keys())[0]
            break
        df = to_dataframe(
            CrystalXMLSource(bench_10mb, row_tag="Details")
            | DropFields([first_key])
        )
        assert first_key not in df.columns

    def test_dataframe_empty(self):
        df = to_dataframe(Pipeline(iter([])))
        assert df.shape == (0, 0)

    def test_dataframe_chunksize_empty(self):
        df = to_dataframe(Pipeline(iter([])), chunksize=100)
        assert df.shape == (0, 0)


class TestToCSV:
    def test_csv_basic(self, bench_10mb):
        with tempfile.NamedTemporaryFile(suffix=".csv", mode="w", delete=False) as f:
            path = f.name
        try:
            to_csv(CrystalXMLSource(bench_10mb, row_tag="Details"), path)
            with open(path) as f:
                reader = csv.DictReader(f)
                rows = list(reader)
            assert len(rows) > 0
        finally:
            Path(path).unlink(missing_ok=True)

    def test_csv_headers_match_keys(self, bench_10mb):
        first_row = next(iter(CrystalXMLSource(bench_10mb, row_tag="Details")))
        expected_keys = list(first_row.keys())
        with tempfile.NamedTemporaryFile(suffix=".csv", mode="w", delete=False) as f:
            path = f.name
        try:
            to_csv(CrystalXMLSource(bench_10mb, row_tag="Details"), path)
            with open(path) as f:
                reader = csv.DictReader(f)
                assert reader.fieldnames == expected_keys
        finally:
            Path(path).unlink(missing_ok=True)

    def test_csv_custom_delimiter(self, bench_10mb):
        with tempfile.NamedTemporaryFile(suffix=".csv", mode="w", delete=False) as f:
            path = f.name
        try:
            to_csv(
                CrystalXMLSource(bench_10mb, row_tag="Details"),
                path,
                delimiter=";",
            )
            with open(path) as f:
                first_line = f.readline()
            assert ";" in first_line
        finally:
            Path(path).unlink(missing_ok=True)

    def test_csv_empty(self):
        with tempfile.NamedTemporaryFile(suffix=".csv", mode="w", delete=False) as f:
            path = f.name
        try:
            to_csv(Pipeline(iter([])), path)
            with open(path) as f:
                content = f.read()
            assert content == ""
        finally:
            Path(path).unlink(missing_ok=True)

    def test_csv_with_stages(self, bench_10mb):
        with tempfile.NamedTemporaryFile(suffix=".csv", mode="w", delete=False) as f:
            path = f.name
        try:
            to_csv(
                CrystalXMLSource(bench_10mb, row_tag="Details")
                | RenameFields({"Level": "level"}),
                path,
            )
            with open(path) as f:
                reader = csv.DictReader(f)
                assert "level" in reader.fieldnames
        finally:
            Path(path).unlink(missing_ok=True)
