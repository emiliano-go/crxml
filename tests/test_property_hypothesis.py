"""Property-based: ragged/sparse/entity-laden XML vs ElementTree oracle.

Covers 1.0.0 bugs: schema discovery on sparse columns, whitespace text nodes.
Uses Hypothesis if available, otherwise falls back to seeded random (still
property-based, just not shrunk).
"""
import random
import string
import xml.etree.ElementTree as ET
from pathlib import Path

import pytest

from crxml import CrystalXMLSource

try:
    from hypothesis import given, strategies as st, settings, HealthCheck
    HAS_HYPOTHESIS = True
except ImportError:
    HAS_HYPOTHESIS = False

# Value pool that stresses entities, unicode, whitespace, empty
VALUE_POOL = [
    "", " ", "\t", "\n", "  hello  ", "A & B", "<10", 'q"q', "café",
    "日本語", "line\ttab", "100.50", "  ", "&lt;", "&amp;", "a&b",
]

FIELD_NAMES = ["alpha", "beta", "gamma", "delta", "café", "x9", "Field0", "F_1"]

def _escape_xml(s: str) -> str:
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;").replace('"', "&quot;").replace("'", "&apos;")

def _gen_xml_bytes(rng: random.Random, n_rows: int, row_tag: str = "Row") -> bytes:
    parts = [b'<?xml version="1.0"?><Report>']
    for i in range(n_rows):
        # Occasionally insert comments / fake rows / Group wrappers
        roll = rng.random()
        if roll < 0.07:
            parts.append(b"<!-- comment with <Row> fake -->")
            continue
        if roll < 0.12:
            parts.append(b"<Group>")
            glen = rng.randint(1, 3)
        else:
            glen = 0
        for _ in range(max(1, glen) if glen else 1):
            # row_tag with optional Level attr (sparse)
            level = f' Level="{i%3}"' if rng.random() < 0.6 else ""
            row_open = f"<{row_tag}{level}>".encode()
            fields = []
            for name in FIELD_NAMES:
                if rng.random() < 0.45:  # ragged: skip field
                    continue
                val = rng.choice(VALUE_POOL)
                # Occasionally make empty element (no FormattedValue)
                if rng.random() < 0.08:
                    fields.append(f'<Field FieldName="{name}"></Field>'.encode())
                elif rng.random() < 0.1:
                    fields.append(f'<Field FieldName="{name}"><FormattedValue></FormattedValue></Field>'.encode())
                else:
                    esc = _escape_xml(val)
                    # Whitespace text nodes: add spaces inside FormattedValue
                    if rng.random() < 0.15:
                        esc = f"  {esc}  "
                    fields.append(f'<Field FieldName="{name}"><FormattedValue>{esc}</FormattedValue></Field>'.encode())
            # Occasionally add Section / Text
            if rng.random() < 0.2:
                fields.append(f'<Section SectionNumber="{rng.randint(0,2)}"/>'.encode())
            if rng.random() < 0.15:
                fields.append(b'<Text Name="Text1"><TextValue>%</TextValue></Text>')
            if not fields:
                fields.append(b'<Field FieldName="alpha"><FormattedValue>0</FormattedValue></Field>')
            parts.append(row_open + b"".join(fields) + f"</{row_tag}>".encode())
            if glen:
                if rng.random() < 0.5:
                    # Need to close Group after glen rows — we do it outside
                    pass
            if glen == 0:
                break
        if glen:
            parts.append(b"</Group>")
    parts.append(b"</Report>")
    return b"".join(parts)

def _oracle(path: Path, row_tag: str = "Row"):
    root = ET.parse(str(path)).getroot()
    rows = []
    for row in root.iter(row_tag):
        d = dict(row.attrib)
        for child in row:
            if child.tag == "Field":
                name = child.get("FieldName")
                v = ""
                for gc in child:
                    if gc.tag == "FormattedValue":
                        v = gc.text or ""
                        break
                # Preserve whitespace exactly as ElementTree does (no strip)
                d[name] = v
            elif child.tag == "Section":
                d["Section"] = child.get("SectionNumber", "")
            elif child.tag == "Text":
                d[child.get("Name", "Text")] = "".join((gc.text or "") for gc in child if gc.tag == "TextValue")
            else:
                d[child.tag] = child.text or ""
        rows.append(d)
    return rows

def _null_fill(rows):
    keys = set()
    for r in rows:
        keys.update(r.keys())
    return [dict.fromkeys(keys, None) | r for r in rows]

def _check_engines(data: bytes, row_tag="Row"):
    import tempfile
    from pathlib import Path as P
    with tempfile.TemporaryDirectory() as td:
        p = P(td) / "prop.xml"
        p.write_bytes(data)
        oracle = _null_fill(_oracle(p, row_tag))
        # stream, columnar, parallel, and bounded (columnar+memory)
        cases = [
            ("stream", {"row_tag": row_tag, "engine": "stream"}),
            ("columnar", {"row_tag": row_tag, "engine": "columnar"}),
            ("parallel", {"row_tag": row_tag, "engine": "parallel", "threads": 2}),
            ("bounded", {"row_tag": row_tag, "memory": "1KB"}),
            ("parallel_bounded", {"row_tag": row_tag, "engine": "parallel", "threads": 2, "memory": "1KB"}),
        ]
        for name, kwargs in cases:
            src = CrystalXMLSource(str(p), **kwargs)
            if name == "stream":
                got = _null_fill(list(src))
            else:
                got = _null_fill(src.to_arrow().to_pylist())
            assert got == oracle, f"{name} diverged: got {got[:1]} vs oracle {oracle[:1]}"

if HAS_HYPOTHESIS:
    @settings(max_examples=50, deadline=None, suppress_health_check=[HealthCheck.too_slow])
    @given(st.integers(min_value=1, max_value=25), st.integers(min_value=0, max_value=10000))
    def test_property_hypothesis(n_rows, seed):
        rng = random.Random(seed)
        data = _gen_xml_bytes(rng, n_rows=n_rows)
        _check_engines(data, row_tag="Row")

    @settings(max_examples=30, deadline=None)
    @given(st.sampled_from(["Row", "Details"]), st.integers(min_value=1, max_value=15), st.integers(min_value=0, max_value=9999))
    def test_property_row_tag_variants(row_tag, n_rows, seed):
        rng = random.Random(seed)
        data = _gen_xml_bytes(rng, n_rows=n_rows, row_tag=row_tag)
        _check_engines(data, row_tag=row_tag)
else:
    def test_property_random_seeded():
        for seed in range(50):
            rng = random.Random(seed)
            data = _gen_xml_bytes(rng, n_rows=rng.randint(5, 30))
            _check_engines(data, row_tag="Row")

    def test_property_row_tag_variants():
        for tag in ["Row", "Details"]:
            for seed in range(15):
                rng = random.Random(seed*10)
                data = _gen_xml_bytes(rng, n_rows=rng.randint(5, 15), row_tag=tag)
                _check_engines(data, row_tag=tag)

def test_whitespace_text_nodes_preserved(tmp_path):
    """Regression for 1.0.0 whitespace stripping bug: FormattedValue with spaces must be preserved."""
    data = b'<Report><Row><Field FieldName="a"><FormattedValue>  hello  </FormattedValue></Field></Row><Row><Field FieldName="a"><FormattedValue>   </FormattedValue></Field></Row></Report>'
    p = tmp_path / "ws.xml"
    p.write_bytes(data)
    oracle = _null_fill(_oracle(p, "Row"))
    for eng in ("stream", "columnar"):
        src = CrystalXMLSource(str(p), row_tag="Row", engine=eng)
        got = _null_fill(list(src) if eng == "stream" else src.to_arrow().to_pylist())
        assert got == oracle, f"whitespace not preserved in {eng}: {got} vs {oracle}"

def test_sparse_schema_discovery(tmp_path):
    """Schema discovery on sparse columns: late-appearing field must create null-filled column."""
    data = b'<Report><Row><Field FieldName="a"><FormattedValue>1</FormattedValue></Field></Row><Row><Field FieldName="b"><FormattedValue>2</FormattedValue></Field></Row><Row><Field FieldName="a"><FormattedValue>3</FormattedValue></Field><Field FieldName="b"><FormattedValue>4</Value></Field></Row></Report>'
    # Note: last row has mismatched tag to test robustness — use valid XML instead
    data = b'<Report><Row><Field FieldName="a"><FormattedValue>1</FormattedValue></Field></Row><Row><Field FieldName="b"><FormattedValue>2</FormattedValue></Field></Row><Row><Field FieldName="a"><FormattedValue>3</FormattedValue></Field><Field FieldName="b"><FormattedValue>4</FormattedValue></Field></Row></Report>'
    p = tmp_path / "sparse.xml"
    p.write_bytes(data)
    oracle = _null_fill(_oracle(p, "Row"))
    for eng in ("stream", "columnar", "parallel"):
        src = CrystalXMLSource(str(p), row_tag="Row", engine=eng)
        got = _null_fill(src.to_arrow().to_pylist() if eng != "stream" else list(src))
        assert got == oracle
