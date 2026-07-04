"""Integration tests for columnar → PyArrow export via C Data Interface.

Requires the native module built with `--features columnar`:
  maturin develop --features columnar
"""

import pyarrow as pa
import pytest

from crxml import _crxml_core as _core

CrxmlReader = _core.CrxmlReader

pytestmark = pytest.mark.skipif(
    not hasattr(_core, "read_to_columnar"),
    reason="current native build does not expose the columnar export API",
)

SAMPLE_XML = b"""\
<R><Row><Field Name="product"><Value>Widget</Value></Field>\
<Field Name="amount"><Value>99.50</Value></Field></Row>\
<Row><Field Name="product"><Value>Gadget</Value></Field>\
<Field Name="amount"><Value>42.00</Value></Field></Row>\
<Row><Field Name="product"><Value>Widget</Value></Field>\
<Field Name="amount"><Value>150.00</Value></Field></Row></R>"""

INT_SAMPLE_XML = b"""\
<R><Row><Field Name="product"><Value>Widget</Value></Field>\
<Field Name="amount"><Value>99</Value></Field></Row>\
<Row><Field Name="product"><Value>Gadget</Value></Field>\
<Field Name="amount"><Value>42</Value></Field></Row>\
<Row><Field Name="product"><Value>Widget</Value></Field>\
<Field Name="amount"><Value>150</Value></Field></Row></R>"""

GROUND_TRUTH_XML = b"""\
<R><Row><Field Name="product"><Value>Widget</Value></Field>\
<Field Name="amount"><Value>99</Value></Field></Row>\
<Row><Field Name="product"><Value>Gadget</Value></Field>\
<Field Name="amount"><Value>42</Value></Field></Row>\
<Row><Field Name="product"><Value>Widget</Value></Field>\
<Field Name="amount"><Value>150</Value></Field>\
<Field Name="late"><Value>tail</Value></Field></Row></R>"""


def test_columnar_default_schema(tmp_path):
    """Default (string) export yields utf8 columns."""
    p = tmp_path / "test.xml"
    p.write_bytes(SAMPLE_XML)
    tbl = _core.read_to_columnar(str(p), row_tag="Row")
    assert tbl.num_rows == 3
    assert tbl.schema.field("product").type == pa.utf8()
    assert tbl.schema.field("amount").type == pa.utf8()


def test_typed_int64_column(tmp_path):
    """field_types={'amount': 'int64'} yields an int64 Arrow column."""
    p = tmp_path / "test.xml"
    p.write_bytes(INT_SAMPLE_XML)
    tbl = _core.read_to_columnar(
        str(p), row_tag="Row",
        field_types={"amount": "int64"},
    )
    assert tbl.schema.field("amount").type == pa.int64()
    col = tbl.column("amount")
    assert col[0].as_py() == 99
    assert col[1].as_py() == 42
    assert col[2].as_py() == 150


def test_typed_float64_column(tmp_path):
    """field_types={'amount': 'float64'} yields a float64 Arrow column."""
    p = tmp_path / "test.xml"
    p.write_bytes(SAMPLE_XML)
    tbl = _core.read_to_columnar(
        str(p), row_tag="Row",
        field_types={"amount": "float64"},
    )
    assert tbl.schema.field("amount").type == pa.float64()
    col = tbl.column("amount")
    assert abs(col[0].as_py() - 99.5) < 1e-9
    assert abs(col[1].as_py() - 42.0) < 1e-9
    assert abs(col[2].as_py() - 150.0) < 1e-9


def test_dictionary_column(tmp_path):
    """dictionary_columns=['product'] yields a dictionary Arrow column."""
    p = tmp_path / "test.xml"
    p.write_bytes(SAMPLE_XML)
    tbl = _core.read_to_columnar(
        str(p), row_tag="Row",
        dictionary_columns=["product"],
    )
    field = tbl.schema.field("product")
    assert pa.types.is_dictionary(field.type)
    assert field.type.value_type == pa.utf8()
    assert field.type.index_type == pa.int32()
    col = tbl.column("product")
    assert col[0].as_py() == "Widget"
    assert col[1].as_py() == "Gadget"
    assert col[2].as_py() == "Widget"


def test_typed_parse_failure_yields_null(tmp_path):
    """Unparseable typed values become null (not a Python error)."""
    xml = b"""<R><Row><Field Name="score"><Value>42</Value></Field></Row>\
<Row><Field Name="score"><Value>N/A</Value></Field></Row>\
<Row><Field Name="score"><Value>100</Value></Field></Row></R>"""
    p = tmp_path / "test.xml"
    p.write_bytes(xml)
    tbl = _core.read_to_columnar(
        str(p), row_tag="Row",
        field_types={"score": "int64"},
    )
    assert tbl.num_rows == 3
    col = tbl.column("score")
    assert col[0].as_py() == 42
    assert col[1].as_py() is None
    assert col[2].as_py() == 100


def test_full_table_ground_truth_matches_all_export_paths(tmp_path):
    """One table-level assertion covering dtype, values, and chunk parity."""
    p = tmp_path / "ground_truth.xml"
    p.write_bytes(GROUND_TRUTH_XML)

    expected = pa.table(
        {
            "product": pa.array(
                ["Widget", "Gadget", "Widget"],
                type=pa.dictionary(pa.int32(), pa.string()),
            ),
            "amount": pa.array([99, 42, 150], type=pa.int64()),
            "late": pa.array([None, None, "tail"], type=pa.utf8()),
        }
    )

    single = _core.read_to_columnar(
        str(p),
        row_tag="Row",
        field_types={"amount": "int64"},
        dictionary_columns=["product"],
    )
    multi = _core.read_to_columnar_multi(
        str(p),
        row_tag="Row",
        num_chunks=2,
        field_types={"amount": "int64"},
        dictionary_columns=["product"],
    )
    parallel = _core.read_to_columnar_par(
        str(p),
        row_tag="Row",
        num_chunks=2,
        field_types={"amount": "int64"},
        dictionary_columns=["product"],
    )

    for table in (single, multi, parallel):
        assert table.equals(expected, check_metadata=False)


def test_filter_constant_eq_columnar(tmp_path):
    """Filter equal constant via columnar engine."""
    p = tmp_path / "test.xml"
    p.write_bytes(SAMPLE_XML)
    tbl = _core.read_to_columnar(
        str(p), row_tag="Row",
        filter={"field": "product", "op": "==", "value": "Widget"},
    )
    assert tbl.num_rows == 2
    assert tbl.column("product").to_pylist() == ["Widget", "Widget"]


def test_filter_constant_ne_columnar(tmp_path):
    """Filter not-equal constant via columnar engine."""
    p = tmp_path / "test.xml"
    p.write_bytes(SAMPLE_XML)
    tbl = _core.read_to_columnar(
        str(p), row_tag="Row",
        filter={"field": "product", "op": "!=", "value": "Widget"},
    )
    assert tbl.num_rows == 1
    assert tbl.column("product").to_pylist() == ["Gadget"]


def test_filter_compare_columnar(tmp_path):
    """Column-to-column filter via columnar engine."""
    xml = b"""<R><Row><Field Name="min"><Value>10</Value></Field>\
<Field Name="max"><Value>20</Value></Field></Row>\
<Row><Field Name="min"><Value>30</Value></Field>\
<Field Name="max"><Value>15</Value></Field></Row></R>"""
    p = tmp_path / "test.xml"
    p.write_bytes(xml)
    tbl = _core.read_to_columnar(
        str(p), row_tag="Row",
        filter={"field_a": "max", "op": ">", "field_b": "min"},
    )
    # First row: max(20) > min(10) → keep. Second: max(15) > min(30) → false → drop.
    assert tbl.num_rows == 1
    assert tbl.column("min").to_pylist() == ["10"]


def test_filter_with_field_mapping(tmp_path):
    """Filter on a renamed field: use the original (raw) field name."""
    p = tmp_path / "test.xml"
    p.write_bytes(SAMPLE_XML)
    tbl = _core.read_to_columnar(
        str(p), row_tag="Row",
        field_mapping={"product": "item"},
        filter={"field": "product", "op": "==", "value": "Gadget"},
    )
    assert tbl.num_rows == 1
    assert tbl.column("item").to_pylist() == ["Gadget"]
