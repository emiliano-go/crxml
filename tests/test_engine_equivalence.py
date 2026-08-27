"""Engine-equivalence: every engine must agree with stream oracle on real and synthetic files.

This is the adapter-guide recommended test: deleted columnar.rs/splitter.rs and moved
engine to external workspace must not change observable results.
"""
import pytest
from pathlib import Path

from crxml import CrystalXMLSource

# Reuse bench_data discovery from conftest (or direct path)
BENCH_DIR = Path(__file__).resolve().parent.parent / "bench_data"
REAL_533MB = BENCH_DIR / "test_533mb.xml"
SYNTH_1GB = BENCH_DIR / "test_1gb.xml"
SYNTH_100MB = BENCH_DIR / "test_100mb.xml"
SYNTH_10MB = BENCH_DIR / "test_10mb.xml"


def _arrow_equal(a, b) -> bool:
    # pyarrow Table equality ignoring metadata, with null-fill normalization
    # Both tables are already null-filled per adapter; check schema + rows
    if a.num_rows != b.num_rows:
        return False
    if set(a.schema.names) != set(b.schema.names):
        return False
    # Sort columns for comparison (schema order may differ)
    cols = sorted(a.schema.names)
    aa = a.select(cols)
    bb = b.select(cols)
    return aa.equals(bb)


@pytest.mark.bench
@pytest.mark.parametrize("path", [
    pytest.param(SYNTH_10MB, id="synth-10mb"),
    pytest.param(SYNTH_100MB, id="synth-100mb"),
    pytest.param(SYNTH_1GB, id="synth-1gb"),
    pytest.param(REAL_533MB, id="real-533mb"),
])
def test_all_engines_agree(path):
    if not Path(path).exists():
        pytest.skip(f"Missing benchmark file: {path}")
    # Reference: stream is the simplest engine (single-thread, no chunking)
    ref = CrystalXMLSource(str(path), row_tag="Details", engine="stream").to_arrow()
    for eng in ("columnar", "parallel"):
        kwargs = {}
        if eng == "parallel":
            kwargs["threads"] = 3
        other = CrystalXMLSource(str(path), row_tag="Details", engine=eng, **kwargs).to_arrow()
        assert _arrow_equal(other, ref), f"engine {eng} diverged from stream on {path.name}: {other.num_rows} vs {ref.num_rows} rows, {other.schema.names} vs {ref.schema.names}"
    # Bounded is not an engine string; it is triggered via memory budget on columnar
    for mem in ("64MB", "1KB"):
        bounded = CrystalXMLSource(str(path), row_tag="Details", engine="columnar", memory=mem).to_arrow()
        assert _arrow_equal(bounded, ref), f"bounded {mem} diverged on {path.name}"
        assert set(bounded.schema.names) == set(ref.schema.names)
        assert _arrow_equal(other, ref), f"engine {eng} diverged from stream on {path.name}: {other.num_rows} vs {ref.num_rows} rows, {other.schema.names} vs {ref.schema.names}"
        # Also check via read() alias
        # Ensure columnar/parallel/bounded produce same column set as stream (sparse columns null-filled)
        assert set(other.schema.names) == set(ref.schema.names)


def test_stream_vs_columnar_vs_parallel_on_small_synthetic(tmp_path):
    # Small synthetic that exercises ragged/sparse without needing large file
    xml = b"""<?xml version=\"1.0\"?><Report>
<Details Level=\"3\"><Section SectionNumber=\"0\"><Field Name=\"a\"><Value>1</Value></Field></Section></Details>
<Details Level=\"3\"><Section SectionNumber=\"0\"><Field Name=\"b\"><Value>2</Value></Field></Section></Details>
<Details Level=\"3\"><Section SectionNumber=\"0\"><Field Name=\"a\"><Value>3</Value></Field><Field Name=\"b\"><Value>4</Value></Field></Section></Details>
</Report>"""
    p = tmp_path / "small.xml"
    p.write_bytes(xml)
    ref = CrystalXMLSource(str(p), row_tag="Details", engine="stream").to_arrow()
    for eng in ("columnar", "parallel"):
        other = CrystalXMLSource(str(p), row_tag="Details", engine=eng).to_arrow()
        assert _arrow_equal(other, ref)
    # Bounded with tiny budget (memory triggers bounded mode, not engine string)
    bounded = CrystalXMLSource(str(p), row_tag="Details", memory="1KB").to_arrow()
    assert _arrow_equal(bounded, ref)
