"""Tests for crxml integration with rypipe."""
import pytest

import rypipe
from crxml import CrystalXMLSource
from crxml.rypipe_adapter import CrystalXMLAdapter


@pytest.fixture
def sample_xml(tmp_path):
    p = tmp_path / "report.xml"
    p.write_text(
        '<?xml version="1.0"?><CrystalReport>'
        '<Row><Field Name="Name"><Value>Alice</Value></Field>'
        '<Field Name="Age"><Value>30</Value></Field></Row>'
        '<Row><Field Name="Name"><Value>Bob</Value></Field>'
        '<Field Name="Age"><Value>25</Value></Field></Row>'
        '</CrystalReport>'
    )
    return p


class TestAdapter:
    def test_adapter_read_returns_table(self, sample_xml):
        adapter = CrystalXMLAdapter()
        table = adapter.read(str(sample_xml), row_tag="Row")
        assert table.num_rows == 2
        assert "Name" in table.column_names
        assert "Age" in table.column_names

    def test_adapter_respects_pushdown_filter(self, sample_xml):
        adapter = CrystalXMLAdapter()
        table = adapter.read(
            str(sample_xml),
            row_tag="Row",
            filter={"field": "Name", "op": "==", "value": "Alice"},
        )
        assert table.num_rows == 1
        assert table.column("Name")[0].as_py() == "Alice"


class TestRegistry:
    def test_crxml_registered_with_rypipe(self, sample_xml):
        table = rypipe.read(str(sample_xml), format="crxml", row_tag="Row")
        assert table.num_rows == 2

    def test_crxml_extension_auto_detected(self, sample_xml):
        table = rypipe.read(str(sample_xml), row_tag="Row")
        assert table.num_rows == 2


class TestSourceStillWorks:
    def test_crystal_xml_source_unchanged(self, sample_xml):
        source = CrystalXMLSource(sample_xml, row_tag="Row")
        rows = list(source)
        assert len(rows) == 2
        assert rows[0]["Name"] == "Alice"
