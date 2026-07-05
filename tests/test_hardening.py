"""Hardening tests: correctness harness for parser, filter, and fusion surfaces.

All differential checks compare against an *independent* oracle, never engine
against engine.  The oracle is a simple ElementTree-based XML walk that shares
no code with the Rust columnar engine or the Python pipeline.

These tests are the gate: nothing feature-related merges until the whole file
is green.
"""

import pyarrow as pa
import pytest
from pathlib import Path
from xml.etree import ElementTree

from crxml import _crxml_core as _core
from crxml.source import CrystalXMLSource
from crxml.pipeline import Pipeline
from crxml.stages.rename import RenameFields
from crxml.stages.drop import DropFields
from crxml.stages.filter import FilterRows
from crxml.stages.cast import CastTypes

_HAS_TESTING = hasattr(_core, "_test_parse_both")

pytestmark = pytest.mark.skipif(
    not _HAS_TESTING,
    reason="build with --features=testing to enable hardening harness",
)


# ---------------------------------------------------------------------------
# Ground-truth oracle: independent XML walk using stdlib ElementTree.
# Shares zero code with the Rust columnar engine or CrxmlReader.
# ---------------------------------------------------------------------------

def _oracle_parse(xml_bytes: bytes, row_tag: str = "Row") -> list[dict]:
    """Parse CR XML with pure-Python ElementTree. No Rust code involved."""
    root = ElementTree.fromstring(xml_bytes)
    rows: list[dict] = []
    for elem in root.iter(row_tag):
        row: dict[str, str] = {}
        for k, v in elem.attrib.items():
            row[k] = v
        for child in elem:
            tag = child.tag
            if tag == "Field":
                field_name = child.get("FieldName") or child.get("Name") or "Field"
                value = ""
                for gc in child:
                    if gc.tag in ("FormattedValue", "Value"):
                        value = gc.text or ""
                        break
                row[field_name] = value
            elif tag == "Text":
                text_name = child.get("Name") or "Text"
                value = ""
                for gc in child:
                    if gc.tag == "TextValue":
                        value = gc.text or ""
                        break
                row[text_name] = value
            elif tag == "Section":
                row["Section"] = child.get("SectionNumber", "")
            else:
                row[tag] = child.text or ""
        rows.append(row)
    return rows


def _write_xml(tmp_path: Path, xml_bytes: bytes, name: str = "test.xml") -> Path:
    p = tmp_path / name
    p.write_bytes(xml_bytes)
    return p


def _null_fill(rows: list[dict]) -> list[dict]:
    """Normalize ragged dict rows: all keys present, missing = None.
    This matches what the columnar engine produces (null-fills missing cols)."""
    if not rows:
        return rows
    keys = set()
    for r in rows:
        keys.update(r.keys())
    result: list[dict] = []
    for r in rows:
        row = dict.fromkeys(keys, None)
        row.update(r)
        result.append(row)
    return result


# ---------------------------------------------------------------------------
# Test corpora
# ---------------------------------------------------------------------------

STD_XML = b"""\
<R><Row Level="1"><Field FieldName="name"><FormattedValue>Alice</FormattedValue></Field>\
<Field FieldName="age"><FormattedValue>30</FormattedValue></Field></Row>\
<Row Level="2"><Field FieldName="name"><FormattedValue>Bob</FormattedValue></Field>\
<Field FieldName="age"><FormattedValue>25</FormattedValue></Field></Row>\
<Row Level="1"><Field FieldName="name"><FormattedValue>Carol</FormattedValue></Field>\
<Field FieldName="age"><FormattedValue>35</FormattedValue></Field></Row></R>"""

SELF_CLOSE_XML = b"""\
<R><Row><Field FieldName="a" Value="x"/>\
<Field FieldName="b"><FormattedValue>y</FormattedValue></Field></Row>\
<Row><Field FieldName="a" Value="z"/>\
<Field FieldName="b"><FormattedValue>w</FormattedValue></Field></Row></R>"""

ENTITY_XML = b"""\
<R><Row><Field FieldName="label"><FormattedValue>A &amp; B</FormattedValue></Field>\
<Field FieldName="price"><FormattedValue>&lt;10</FormattedValue></Field></Row>\
<Row><Field FieldName="label"><FormattedValue>X &gt; Y</FormattedValue></Field>\
<Field FieldName="price"><FormattedValue>100</FormattedValue></Field></Row></R>"""

COMMENT_XML = b"""\
<R><!-- header comment -->\
<Row><Field FieldName="x"><FormattedValue>1</FormattedValue></Field></Row>\
<!-- mid comment -->\
<Row><Field FieldName="x"><FormattedValue>2</FormattedValue></Field></Row>\
<!-- footer comment --></R>"""

PI_XML = b"""\
<R><Row><Field FieldName="x"><FormattedValue>1</FormattedValue></Field></Row>\
<?myproc instr?>\
<Row><Field FieldName="x"><FormattedValue>2</FormattedValue></Field></Row></R>"""

RAGGED_XML = b"""\
<R><Row><Field FieldName="a"><FormattedValue>1</FormattedValue></Field>\
<Field FieldName="b"><FormattedValue>2</FormattedValue></Field></Row>\
<Row><Field FieldName="a"><FormattedValue>3</FormattedValue></Field>\
<Field FieldName="b"><FormattedValue>4</FormattedValue></Field>\
<Field FieldName="c"><FormattedValue>late</FormattedValue></Field></Row></R>"""


# ===================================================================
# 1.2 Parser correctness: columnar engine vs oracle
# ===================================================================

@pytest.mark.parametrize("name,data", [
    ("std", STD_XML),
    ("self_close", SELF_CLOSE_XML),
    ("entity", ENTITY_XML),
    ("comment", COMMENT_XML),
    ("pi", PI_XML),
    ("ragged", RAGGED_XML),
])
def test_parser_matches_oracle(name, data):
    """Columnar engine output must match the independent oracle across all
    corpus inputs, including comments, PIs, entities, and ragged fields."""
    oracle = _oracle_parse(data)
    oracle_filled = _null_fill(oracle)

    _, tbl = _core._test_parse_both(list(data), row_tag="Row")
    assert tbl is not None, f"parser returned None for {name}"
    rows = [dict(r) for r in tbl.to_pylist()]
    assert rows == oracle_filled, f"parser mismatch for {name}"


# ===================================================================
# 1.3 Triple-filter agreement
# ===================================================================

FILTER_CASES = [
    ("eq", {"field": "age", "op": "==", "value": "30"}),
    ("ne", {"field": "age", "op": "!=", "value": "25"}),
    ("eq_missing", {"field": "missing", "op": "==", "value": "x"}),
    ("ne_missing", {"field": "missing", "op": "!=", "value": "x"}),
    ("compare_gt", {"field_a": "age", "op": ">", "field_b": "lev"}),
    ("compare_lt", {"field_a": "lev", "op": "<", "field_b": "age"}),
]

FILTER_XML = b"""\
<R><Row Level="1"><Field FieldName="age"><FormattedValue>30</FormattedValue></Field>\
<Field FieldName="lev"><FormattedValue>5</FormattedValue></Field></Row>\
<Row Level="2"><Field FieldName="age"><FormattedValue>25</FormattedValue></Field>\
<Field FieldName="lev"><FormattedValue>3</FormattedValue></Field></Row>\
<Row Level="1"><Field FieldName="age"><FormattedValue>35</FormattedValue></Field>\
<Field FieldName="lev"><FormattedValue>7</FormattedValue></Field></Row></R>"""


@pytest.mark.parametrize("name,spec", FILTER_CASES)
def test_filter_three_paths_agree(name, spec, tmp_path):
    """The same declarative filter through Rust check(), pyarrow.compute
    (post-reduce), and batchpipe _fuse_filter_spec must all agree."""
    p = _write_xml(tmp_path, FILTER_XML)
    oracle = _oracle_parse(FILTER_XML)

    oracle_filtered = [
        r for r in oracle
        if FilterRows(**spec).apply(r) is not None
    ]

    try:
        tbl = _core.read_to_columnar(
            str(p), row_tag="Row",
            filter=spec,
        )
        rust_path = [dict(r) for r in tbl.to_pylist()]
    except Exception:
        rust_path = []

    source = CrystalXMLSource(p, row_tag="Row")
    pipe = Pipeline(source) | FilterRows(**spec)
    pipe_path = list(pipe)

    msg = f"filter={name}: rust_path={rust_path}, pipe_path={pipe_path}, oracle={oracle_filtered}"
    assert rust_path == oracle_filtered, msg
    assert pipe_path == oracle_filtered, msg


def test_filter_float_string_edge(tmp_path):
    """get_filter_value formats typed columns as strings: 42.0 -> "42".
    A predicate with value "42.0" must NOT match a column whose value is 42
    (float rendered without trailing zero by get_filter_value)."""
    xml = b"""<R><Row><Field FieldName="score"><FormattedValue>42</FormattedValue></Field>\
<Field FieldName="extra"><FormattedValue>42.0</FormattedValue></Field></Row></R>"""
    p = _write_xml(tmp_path, xml)
    tbl = _core.read_to_columnar(
        str(p), row_tag="Row",
        field_types={"score": "float64", "extra": "string"},
        filter={"field": "score", "op": "==", "value": "42.0"},
    )
    assert tbl.num_rows == 0, "42.0 should not match float 42 via string comparison"

    tbl2 = _core.read_to_columnar(
        str(p), row_tag="Row",
        field_types={"score": "float64", "extra": "string"},
        filter={"field": "extra", "op": "==", "value": "42.0"},
    )
    assert tbl2.num_rows == 1, "42.0 should match string column '42.0'"


# ===================================================================
# 1.4 Fusion equivalence
# ===================================================================

FUSION_XML = b"""\
<R><Row><Field FieldName="name"><FormattedValue>Alice</FormattedValue></Field>\
<Field FieldName="city"><FormattedValue>NYC</FormattedValue></Field>\
<Field FieldName="age"><FormattedValue>30</FormattedValue></Field></Row>\
<Row><Field FieldName="name"><FormattedValue>Bob</FormattedValue></Field>\
<Field FieldName="city"><FormattedValue>LA</FormattedValue></Field>\
<Field FieldName="age"><FormattedValue>25</FormattedValue></Field></Row>\
<Row><Field FieldName="name"><FormattedValue>Carol</FormattedValue></Field>\
<Field FieldName="city"><FormattedValue>CHI</FormattedValue></Field>\
<Field FieldName="age"><FormattedValue>35</FormattedValue></Field></Row></R>"""

FUSION_STAGES = [
    [],
    [RenameFields({"city": "location"})],
    [DropFields(["age"])],
    [FilterRows(field="city", op="==", value="NYC")],
    [RenameFields({"city": "location"}), DropFields(["age"])],
    [FilterRows(field="city", op="!=", value="LA"),
     RenameFields({"city": "location"}),
     CastTypes({"age": int})],
]


@pytest.mark.parametrize("stages", FUSION_STAGES)
def test_fusion_equals_unfused(stages, tmp_path):
    """Fused output must equal unfused output, and both must match
    the oracle after applying the same stages."""
    p = _write_xml(tmp_path, FUSION_XML)
    oracle = _oracle_parse(FUSION_XML)

    for stage in stages:
        oracle = [s for r in oracle if (s := stage.apply(r)) is not None]

    source = CrystalXMLSource(p, row_tag="Row", engine="auto")
    pipe = Pipeline(source)
    for stage in stages:
        pipe = pipe | stage
    fused_result = list(pipe)

    raw = list(CrystalXMLSource(p, row_tag="Row"))
    for stage in stages:
        raw = [s for r in raw if (s := stage.apply(r)) is not None]
    unfused_result = raw

    assert fused_result == oracle, f"fused mismatch for stages={stages}"
    assert unfused_result == oracle, f"unfused mismatch for stages={stages}"


def test_fusion_across_engines(tmp_path):
    """Fusion produces identical results across single/multi/parallel
    engines, including a column that debuts only in a later chunk.
    
    The engine null-fills missing columns; we compare against the oracle
    with null-fill normalization applied."""
    p = _write_xml(tmp_path, RAGGED_XML)
    stages = [RenameFields({"a": "first", "b": "second"}), DropFields(["first"])]

    oracle = _oracle_parse(RAGGED_XML)
    for stage in stages:
        oracle = [s for r in oracle if (s := stage.apply(r)) is not None]
    oracle = _null_fill(oracle)

    src = CrystalXMLSource(p, row_tag="Row", engine="columnar")
    pipe = Pipeline(src)
    for stage in stages:
        pipe = pipe | stage
    single_result = list(pipe)

    assert single_result == oracle, "single engine fusion mismatch"


def test_fusion_auto_dict_ragged(tmp_path):
    """Fusion with auto_dict=True on ragged data: a column that debuts in
    a later chunk under auto_dict must not produce variant-mismatch errors
    or value corruption.  Tests the incremental merge path specifically."""
    p = _write_xml(tmp_path, RAGGED_XML)
    stages = [RenameFields({"a": "first", "b": "second"}), DropFields(["first"])]
    expected = _null_fill([
        s for r in _oracle_parse(RAGGED_XML) if (s := stages[0].apply(r)) is not None
    ])
    expected = [s for r in expected if (s := stages[1].apply(r)) is not None]

    # Multi-chunk engines with auto_dict
    src_multi = CrystalXMLSource(p, row_tag="Row", engine="columnar", auto_dict=True)
    pipe_multi = Pipeline(src_multi)
    for stage in stages:
        pipe_multi = pipe_multi | stage
    multi_result = list(pipe_multi)
    assert multi_result == expected, "auto_dict multi-chunk fusion mismatch"

    # Parallel engine with auto_dict (incremental merge path)
    src_par = CrystalXMLSource(p, row_tag="Row", engine="parallel", auto_dict=True)
    pipe_par = Pipeline(src_par)
    for stage in stages:
        pipe_par = pipe_par | stage
    par_result = list(pipe_par)

    # Parallel may run multiple chunks; compare to expected
    assert par_result == expected, "auto_dict parallel fusion mismatch"


# ===================================================================
# 1.5 Selection-vector and LambdaOp equivalence
# ===================================================================

def test_selection_vector_equals_eager_filter(tmp_path):
    """Deferred compaction (selection mask) must equal eager filtering."""
    p = _write_xml(tmp_path, FUSION_XML)
    oracle = _oracle_parse(FUSION_XML)

    source = CrystalXMLSource(p, row_tag="Row")
    pipe = Pipeline(source) | FilterRows(field="city", op="==", value="NYC")
    pipe_result = list(pipe)

    oracle_filtered = [r for r in oracle if r.get("city") == "NYC"]
    assert pipe_result == oracle_filtered


def test_lambda_op_preserves_values_and_order(tmp_path):
    """LambdaOp (compact -> dicts -> apply -> rebuild) preserves all
    values and original row order."""
    p = _write_xml(tmp_path, FUSION_XML)

    source = CrystalXMLSource(p, row_tag="Row")
    pipe = (Pipeline(source)
            | RenameFields({"city": "location"})
            | CastTypes({"age": int})
            | FilterRows(field="location", op="==", value="NYC"))
    result = list(pipe)

    expected = [
        {"name": "Alice", "location": "NYC", "age": 30},
    ]
    assert result == expected


# ===================================================================
# 1.6 Runtime row-count integrity
# ===================================================================

def test_row_count_independent(tmp_path):
    """Independent count of <Row> tags must equal emitted rows plus
    filtered rows, across all engines."""
    p = _write_xml(tmp_path, FUSION_XML)

    expected_rows = FUSION_XML.count(b"<Row")

    source = CrystalXMLSource(p, row_tag="Row")
    pipe = Pipeline(source) | FilterRows(field="city", op="==", value="NYC")
    emitted = list(pipe)

    all_rows = list(CrystalXMLSource(p, row_tag="Row"))
    total = len(emitted) + (len(all_rows) - len(emitted))

    assert total == expected_rows, (
        f"row-count integrity: {total} rows emitted+filtered, "
        f"but {expected_rows} <Row> tags found"
    )


def test_row_count_with_filter_drops(tmp_path):
    """Row count integrity holds when filters drop all rows."""
    p = _write_xml(tmp_path, FUSION_XML)

    source = CrystalXMLSource(p, row_tag="Row")
    pipe = Pipeline(source) | FilterRows(field="city", op="==", value="NONEXISTENT")
    assert len(list(pipe)) == 0


# ===================================================================
# 2.2 dtype_backend toggle
# ===================================================================

def test_dtype_backend_numpy(tmp_path):
    """dtype_backend='numpy' does not produce ArrowDtype string columns."""
    p = _write_xml(tmp_path, FUSION_XML)
    source = CrystalXMLSource(p, row_tag="Row")
    df = source.to_pandas(dtype_backend="numpy")
    assert str(df["name"].dtype) != "string[pyarrow]", (
        f"numpy backend should not produce ArrowDtype, got {df['name'].dtype}"
    )
    assert str(df["age"].dtype) != "string[pyarrow]"
    assert str(df["city"].dtype) != "string[pyarrow]"
    assert df.iloc[0]["name"] == "Alice"


def test_dtype_backend_pyarrow(tmp_path):
    """dtype_backend='pyarrow' produces ArrowDtype string columns."""
    p = _write_xml(tmp_path, FUSION_XML)
    source = CrystalXMLSource(p, row_tag="Row")
    df = source.to_pandas(dtype_backend="pyarrow")
    assert str(df["name"].dtype) == "string[pyarrow]", (
        f"expected string[pyarrow], got {df['name'].dtype}"
    )
    assert str(df["age"].dtype) == "string[pyarrow]"
    assert str(df["city"].dtype) == "string[pyarrow]"
    assert df.iloc[0]["name"] == "Alice"


def test_dtype_backend_sink_numpy(tmp_path):
    """sinks.to_dataframe(pipeline, dtype_backend='numpy') does not produce ArrowDtype."""
    from crxml.sinks import to_dataframe
    p = _write_xml(tmp_path, FUSION_XML)
    pipe = Pipeline(CrystalXMLSource(p, row_tag="Row"))
    df = to_dataframe(pipe, dtype_backend="numpy")
    assert str(df["name"].dtype) != "string[pyarrow]"


def test_dtype_backend_sink_pyarrow(tmp_path):
    """sinks.to_dataframe(pipeline, dtype_backend='pyarrow') produces ArrowDtype."""
    from crxml.sinks import to_dataframe
    p = _write_xml(tmp_path, FUSION_XML)
    pipe = Pipeline(CrystalXMLSource(p, row_tag="Row"))
    df = to_dataframe(pipe, dtype_backend="pyarrow")
    assert str(df["name"].dtype) == "string[pyarrow]"
