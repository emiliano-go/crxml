"""Cross-engine differential tests.

Every engine (stream, columnar, parallel, bounded, pushdown pipelines)
must agree with an independent ElementTree oracle on randomized CR-like
XML, including ragged fields, empty elements, entities, unicode,
comments containing fake row tags, and Group wrappers that stress chunk
seams.
"""
import random
import xml.etree.ElementTree as ET
from pathlib import Path

import pytest

from crxml import (
    CrystalXMLSource,
    Pipeline,
    RenameFields,
    DropFields,
    CastTypes,
    FilterRows,
)

FIELD_NAMES = ["alpha", "beta", "gamma", "delta", "café", "x9"]
VALUE_POOL = [
    "1", "42", "hello world", "", "A & B", "<10", 'q"q', "café",
    "日本語", "line\ttab", "100.50",
]


def _gen_xml(rng: random.Random, n_rows: int) -> bytes:
    """Build a CR-like XML document with adversarial-but-legal features."""
    parts = [b'<?xml version="1.0" encoding="UTF-8"?>\n<Report>']
    i = 0
    while i < n_rows:
        roll = rng.random()
        if roll < 0.08:
            parts.append(b"<!-- comment mentioning <Row> inside -->")
            continue
        if roll < 0.12:
            parts.append(b"<Group GroupName=\"g\">")
            group_len = rng.randint(2, 4)
        else:
            group_len = 0
        for _ in range(group_len):
            parts.append(_gen_row(rng, i))
            i += 1
            if i >= n_rows:
                break
        if group_len:
            parts.append(b"</Group>")
    parts.append(b"</Report>")
    return b"".join(parts)


def _gen_row(rng: random.Random, i: int) -> bytes:
    attrs = b' Level="%d"' % (i % 3) if rng.random() < 0.7 else b""
    fields = []
    for name in FIELD_NAMES:
        if rng.random() < 0.35:  # ragged: skip this field entirely
            continue
        val = rng.choice(VALUE_POOL)
        esc = (
            val.replace("&", "&amp;")
            .replace("<", "&lt;")
            .replace(">", "&gt;")
            .replace('"', "&quot;")
        )
        fields.append(
            f'<Field FieldName="{name}">'
            f"<FormattedValue>{esc}</FormattedValue></Field>".encode()
        )
    if not fields:
        fields.append(b'<Field FieldName="alpha"><FormattedValue>0</FormattedValue></Field>')
    return b"<Row" + attrs + b">" + b"".join(fields) + b"</Row>"


def _oracle_rows(data: bytes) -> list[dict]:
    """Independent reference parse via ElementTree."""
    root = ET.fromstring(data)
    rows = []
    for row in root.iter("Row"):
        d = dict(row.attrib)
        for child in row:
            if child.tag == "Field":
                name = child.get("FieldName")
                value = ""
                for gc in child:
                    if gc.tag == "FormattedValue":
                        value = gc.text or ""
                        break
                d[name] = value
            elif child.tag == "Section":
                d["Section"] = child.get("SectionNumber", "")
            else:
                d[child.tag] = child.text or ""
        rows.append(d)
    return rows


def _null_fill(rows: list[dict]) -> list[dict]:
    keys: set = set()
    for r in rows:
        keys.update(r.keys())
    out = []
    for r in rows:
        row = dict.fromkeys(keys, None)
        row.update(r)
        out.append(row)
    return out


def _write(tmp_path: Path, data: bytes, name: str) -> Path:
    p = tmp_path / name
    p.write_bytes(data)
    return p


SEEDS = [11, 22, 33, 44]


@pytest.mark.parametrize("seed", SEEDS)
def test_engines_match_oracle(tmp_path, seed):
    rng = random.Random(seed)
    data = _gen_xml(rng, n_rows=60)
    assert len(data) > 1024, "generator produced a tiny file"
    path = _write(tmp_path, data, "diff.xml")
    expected = _null_fill(_oracle_rows(data))

    stream_rows = _null_fill(list(CrystalXMLSource(path, row_tag="Row", engine="stream")))
    assert stream_rows == expected, "stream engine diverged from oracle"

    col = CrystalXMLSource(path, row_tag="Row", engine="columnar")
    assert _null_fill(col.to_arrow().to_pylist()) == expected, "columnar diverged"

    par = CrystalXMLSource(path, row_tag="Row", engine="parallel", threads=3)
    assert _null_fill(par.to_arrow().to_pylist()) == expected, "parallel diverged"

    bounded = CrystalXMLSource(path, row_tag="Row", engine="columnar", memory="1KB")
    assert _null_fill(bounded.to_arrow().to_pylist()) == expected, "bounded diverged"

    par_bounded = CrystalXMLSource(
        path, row_tag="Row", engine="parallel", threads=3, memory="1KB"
    )
    assert _null_fill(par_bounded.to_arrow().to_pylist()) == expected, \
        "parallel+bounded diverged"


@pytest.mark.parametrize("seed", SEEDS)
def test_pushdown_pipeline_matches_dict_fusion(tmp_path, seed):
    """Declarative-stage pipelines (Rust pushdown) must produce the same
    rows as applying the identical stages as plain dict transforms."""
    rng = random.Random(seed + 100)
    data = _gen_xml(rng, n_rows=60)
    path = _write(tmp_path, data, "push.xml")

    stages = [
        RenameFields({"alpha": "ALPHA", "café": "CAFE"}),
        DropFields(["gamma"]),
        CastTypes({"Level": int}),
        FilterRows(field="delta", op="==", value="hello world"),
    ]

    # Reference: dict-level sequential application over oracle rows
    # (raw dicts: missing fields are absent keys, exactly as parsers
    # emit them). Null-fill only for the final comparison.
    rows = _oracle_rows(data)
    for st in stages:
        rows = list(st(rows))

    got = list(
        CrystalXMLSource(path, row_tag="Row")
        | stages[0] | stages[1] | stages[2] | stages[3]
    )
    # The Arrow side keeps columns for fields that exist anywhere in the
    # file (all-null after filtering); the dict side just omits those keys.
    # Normalize both sides onto the shared key union before comparing.
    keys = set()
    for r in got:
        keys.update(r)
    for r in rows:
        keys.update(r)

    def fill(rs):
        return [{**dict.fromkeys(keys, None), **r} for r in rs]

    assert fill(got) == fill(rows)


def test_empty_and_missing_text_agree(tmp_path):
    """Empty FormattedValue and childless Field elements mean 'present,
    empty string' in every engine (regression for the parse_tail guard)."""
    data = (
        b'<R>'
        b'<Row><Field FieldName="a"><FormattedValue></FormattedValue></Field>'
        b'<Field FieldName="b"><FormattedValue>1</FormattedValue></Field></Row>'
        b'<Row><Field FieldName="a"></Field>'
        b'<Field FieldName="b"><FormattedValue>2</FormattedValue></Field></Row>'
        b'</R>'
    )
    path = _write(tmp_path, data, "empty.xml")
    expected = _null_fill(_oracle_rows(data))

    stream = _null_fill(list(CrystalXMLSource(path, row_tag="Row", engine="stream")))
    col = _null_fill(
        CrystalXMLSource(path, row_tag="Row", engine="columnar").to_arrow().to_pylist()
    )
    par = _null_fill(
        CrystalXMLSource(path, row_tag="Row", engine="parallel", threads=2)
        .to_arrow()
        .to_pylist()
    )
    assert stream == expected
    assert col == expected
    assert par == expected


def test_seam_stress_many_chunks(tmp_path):
    """Thread count far above row count: splits land inside Group blocks."""
    rng = random.Random(7)
    data = _gen_xml(rng, n_rows=25)
    path = _write(tmp_path, data, "seam.xml")
    expected = _null_fill(_oracle_rows(data))
    for threads in (1, 4, 16):
        src = CrystalXMLSource(path, row_tag="Row", engine="parallel", threads=threads)
        assert _null_fill(src.to_arrow().to_pylist()) == expected, f"threads={threads}"


def test_empty_file_all_engines(tmp_path):
    path = _write(tmp_path, b"", "empty_file.xml")
    assert list(CrystalXMLSource(path, row_tag="Row", engine="stream")) == []
    t = CrystalXMLSource(path, row_tag="Row", engine="columnar").to_arrow()
    assert t.num_rows == 0


def test_no_rows_all_engines(tmp_path):
    data = b"<R><!-- nothing here --></R>"
    path = _write(tmp_path, data, "norows.xml")
    assert list(CrystalXMLSource(path, row_tag="Row", engine="stream")) == []
    t = CrystalXMLSource(path, row_tag="Row", engine="columnar").to_arrow()
    assert t.num_rows == 0


def test_sparse_disjoint_columns_all_engines(tmp_path):
    """Rows with completely disjoint field sets — the 533 MB real export pattern.

    Field72 appears in 8% of rows, Text21 in 1% (sparse). The stream engine
    used first-row columns as schema and crashed with KeyError when later rows
    had fields not in row 0. Regression test for source.py:295.
    """
    data = (
        b'<?xml version="1.0"?>\n<R>'
        # Row 0: only field 'a'
        b'<Row><Field FieldName="a"><FormattedValue>1</FormattedValue></Field></Row>'
        # Row 1: only field 'b' (disjoint from row 0)
        b'<Row><Field FieldName="b"><FormattedValue>2</FormattedValue></Field></Row>'
        # Row 2: fields 'a' and 'b'
        b'<Row><Field FieldName="a"><FormattedValue>3</FormattedValue></Field>'
        b'<Field FieldName="b"><FormattedValue>4</FormattedValue></Field></Row>'
        # Row 3: only field 'c' (new field, never seen before)
        b'<Row><Field FieldName="c"><FormattedValue>5</FormattedValue></Field></Row>'
        # Row 4: only field 'a'
        b'<Row><Field FieldName="a"><FormattedValue>6</FormattedValue></Field></Row>'
        # Row 5: fields 'b' and 'c' (no 'a')
        b'<Row><Field FieldName="b"><FormattedValue>7</FormattedValue></Field>'
        b'<Field FieldName="c"><FormattedValue>8</FormattedValue></Field></Row>'
        b'</R>'
    )
    path = _write(tmp_path, data, "sparse.xml")
    expected = _null_fill(_oracle_rows(data))

    # All three engines must produce identical output
    stream = _null_fill(list(CrystalXMLSource(path, row_tag="Row", engine="stream")))
    col = _null_fill(
        CrystalXMLSource(path, row_tag="Row", engine="columnar")
        .to_arrow()
        .to_pylist()
    )
    par = _null_fill(
        CrystalXMLSource(path, row_tag="Row", engine="parallel", threads=2)
        .to_arrow()
        .to_pylist()
    )
    bounded = _null_fill(
        CrystalXMLSource(path, row_tag="Row", engine="columnar", memory="1KB")
        .to_arrow()
        .to_pylist()
    )
    assert stream == expected, "stream diverged on sparse disjoint columns"
    assert col == expected, "columnar diverged on sparse disjoint columns"
    assert par == expected, "parallel diverged on sparse disjoint columns"
    assert bounded == expected, "bounded diverged on sparse disjoint columns"


# ===========================================================================
# Filtered differential tests — exercises the predicate-first path.
# Bugs 3 & 4 (silent value loss via normalize) would be caught here.
# ===========================================================================

def _gen_filtered_xml(rng: random.Random, n_rows: int) -> bytes:
    """Generate CR XML with 11 fields per row.

    - Level attribute (0/1/2) — position 0
    - alpha..kappa (11 fields) — positions 1-11
    - kappa (Field11) appears in only ~30% of rows (sparse)
    - Row 0 has no alpha field (predicate column absent)
    """
    parts = [b'<?xml version="1.0"?>\n<Report>']
    for i in range(n_rows):
        attrs = f' Level="{i % 3}"'
        fields = []
        # alpha is absent in row 0 (predicate column missing)
        if i != 0:
            fields.append(f"<Field FieldName=\"alpha\"><FormattedValue>{i % 10}</FormattedValue></Field>")
        for name in ["beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota"]:
            fields.append(f"<Field FieldName=\"{name}\"><FormattedValue>{rng.randint(0, 9)}</FormattedValue></Field>")
        # kappa (sparse): only ~30% of rows
        if rng.random() < 0.3:
            fields.append(f"<Field FieldName=\"kappa\"><FormattedValue>{rng.randint(0, 5)}</FormattedValue></Field>")
        parts.append((f"<Row{attrs}>" + "".join(fields) + "</Row>").encode())
    parts.append(b"</Report>")
    return b"".join(parts)


def _oracle_filter(data: bytes, field: str, op: str, value: str) -> list[dict]:
    """Reference: parse with ET, apply filter, null-fill."""
    rows = _oracle_rows(data)
    filtered = []
    for r in rows:
        actual = r.get(field)
        if op == "==":
            if actual == value:
                filtered.append(r)
        elif op == "!=":
            if actual != value:
                filtered.append(r)
    return _null_fill(filtered)


ENGINES = ["stream", "columnar", "parallel", "bounded"]
FILTERS = [
    # (field, op, value, position, selectivity)
    ("alpha", "==", "5", "first", "~50%"),    # alpha present ~50% of rows, value 5 in ~10%
    ("alpha", "!=", "x", "first", "100%"),     # 100% pass
    ("alpha", "==", "NONEXISTENT", "first", "0%"),  # 0% pass
    ("delta", "==", "5", "middle", "~50%"),
    ("delta", "!=", "x", "middle", "100%"),
    ("delta", "==", "NONEXISTENT", "middle", "0%"),
    ("kappa", "==", "3", "last", "~30%"),      # sparse column
    ("kappa", "!=", "x", "last", "~30%"),      # keep rows where kappa != x
    ("kappa", "==", "NONEXISTENT", "last", "0%"),
]


@pytest.mark.parametrize("field,op,value,position,sel", FILTERS)
@pytest.mark.parametrize("engine", ENGINES)
def test_filtered_output_matches_reference(tmp_path, field, op, value, position, sel, engine):
    """Filtered output must match an independent ET oracle.

    Covers the three predicate positions (first/middle/last), three
    selectivities (0%/~50%/100%), four engines, plus a sparse column
    (kappa) and a row where the predicate column is absent (row 0).
    """
    rng = random.Random(42)
    data = _gen_filtered_xml(rng, n_rows=60)
    path = _write(tmp_path, data, f"filter_{field}_{op}_{sel}.xml")
    expected = _oracle_filter(data, field, op, value)

    filt = {"field": field, "op": op, "value": value}
    if engine == "stream":
        # Stream iterates dicts; filter manually to avoid to_arrow on sparse cols
        all_rows = list(CrystalXMLSource(path, row_tag="Row", engine="stream"))
        if op == "==":
            filtered = [r for r in all_rows if r.get(field) == value]
        else:
            filtered = [r for r in all_rows if r.get(field) != value]
        got = _null_fill(filtered)
    elif engine == "columnar":
        got = _null_fill(
            CrystalXMLSource(path, row_tag="Row", engine="columnar", filter=filt)
            .to_arrow().to_pylist()
        )
    elif engine == "parallel":
        got = _null_fill(
            CrystalXMLSource(path, row_tag="Row", engine="parallel", threads=2, filter=filt)
            .to_arrow().to_pylist()
        )
    elif engine == "bounded":
        got = _null_fill(
            CrystalXMLSource(path, row_tag="Row", engine="columnar", memory="1KB", filter=filt)
            .to_arrow().to_pylist()
        )

    assert len(got) == len(expected), (
        f"{engine} {field} {op} {value}: got {len(got)} rows, expected {len(expected)}"
    )
    # Compare values — the check that would have caught Bugs 3 & 4
    for i, (g, e) in enumerate(zip(got, expected)):
        for k in set(list(g.keys()) + list(e.keys())):
            gv = g.get(k)
            ev = e.get(k)
        assert g == e, (
            f"{engine} {field} {op} {value} row {i}:\n  got:      {g}\n  expected: {e}"
        )
