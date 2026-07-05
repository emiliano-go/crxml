"""
Benchmark suite for crxml.
Generates 10 MB, 50 MB, and 100 MB synthetic Crystal Reports XML files
with random data, then measures throughput and memory usage.
"""

import os
import random
import time
import subprocess
import sys
from pathlib import Path
from xml.sax.saxutils import escape

try:
    import resource  # Unix-only
except ImportError:
    resource = None

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE / "src"))
import importlib
_core = importlib.import_module("crxml._crxml_core")
OUT_DIR = HERE / "bench_data"
OUT_DIR.mkdir(exist_ok=True)

random.seed(42)

NAMES = [
    "Distribuidora del Sur S.A.", "Comercial Norte Ltda.",
    "Importadora Pacifico SpA", "Alimentos y Bebidas Mendoza",
    "Ferreteria Industrial Lopez", "Supermercados Unidos S.A.",
    "Logistica Express C.A.", "Textiles del Valle EIRL",
    "Laboratorios Farmaceuticos Beta", "Autopartes y Servicios GT",
]
PRODUCTS = [
    "Aceitunas Verdes Rellenas Pote 900g", "Harina de Trigo x 1kg",
    "Galletas de Arroz Sin Gluten 100g", "Chocolates Artesanales 250g",
    "Aceite de Oliva Extra Virgen 500ml", "Pasta Spaghetti Integral 500g",
    "Mermelada de Frutilla 280g", "Cafe Molido Premium 250g",
    "Te Verde en Hebras 100g", "Arroz Parbolizado 1kg",
    "Lentejas Secas Bolsa 500g", "Atun al Natural Lata 180g",
    "Queso Rallado Parmesano 120g", "Yogur Natural Batido 1L",
    "Jugo de Naranja Concentrado 1L",
]
ARTICULOS = [
    "01-00123", "01-00456", "02-00789", "03-00111", "03-00222",
    "05-00333", "05-00444", "08-00555", "09-00666", "11-00777",
    "13-00888", "15-00999", "17-00100", "20-00200", "25-00300",
]


def rint(a=1, b=99999):
    return random.randint(a, b)


def rflt(lo=0.01, hi=99999.99):
    return round(random.uniform(lo, hi), 2)


def fmt(v):
    return f"{v:,.2f}"


def raw(v):
    return f"{v:.2f}"


def rand_persona():
    return f"{rint(10000,99999)} {random.choice(NAMES)}"


def rand_desc():
    return random.choice(PRODUCTS)


def rand_art():
    return random.choice(ARTICULOS)


def rand_doc():
    return random.choice(["Vta.Cred.", "Vta.Cont.", "N.Cred.", "N.Deb."])


NS = "urn:crystal-reports:schemas:report-detail"

HEAD = f"""<?xml version="1.0" encoding="UTF-8" ?>
<CrystalReport xmlns="{NS}" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
<Group Level="1">
<GroupHeader>
<Section SectionNumber="0">
<Text Name="Text12"><TextValue>Fecha :</TextValue>
</Text>
<Field Name="Field3" FieldName="{{lstDiarioVentas.Fecha}}"><FormattedValue>01/01/2024 00:00:00</FormattedValue><Value>2024-01-01T00:00:00</Value></Field>
</Section>
</GroupHeader>
"""

TAIL = """</Group>
</CrystalReport>
"""


def make_field(name, fieldname, formatted, value):
    return (
        f'<Field Name="{name}" FieldName="{fieldname}">'
        f"<FormattedValue>{escape(formatted)}</FormattedValue>"
        f"<Value>{escape(value)}</Value>"
        f"</Field>"
    )


def make_group_header():
    gh_fields = [
        make_field("Field24", "{@FNroBoleta}", chr(rint(65,90)), chr(rint(65,90))),
        make_field("Field9", "{@FSTotal}", fmt(rflt(100,500000)), raw(rflt(100,500000))),
        make_field("Field11", "{lstDiarioVentas.TotImp2}", fmt(rflt(10,100000)), raw(rflt(10,100000))),
        make_field("Field21", "{lstDiarioVentas.Total}", fmt(rflt(100,600000)), raw(rflt(100,600000))),
        make_field("Field37", "{lstDiarioVentas.Redondeo}", fmt(rflt(-1,1)), raw(rflt(-1,1))),
        make_field("Field25", "{lstDiarioVentas.NumeroDoc}", str(rint(60000,99999)), str(rint(60000,99999))),
        make_field("Field7", "{lstDiarioVentas.Exento}", fmt(rflt(0,5000)), raw(rflt(0,5000))),
        make_field("Field6", "{@Docum}", rand_doc(), rand_doc()),
        make_field("Field4", "{@Persona}", rand_persona(), rand_persona()),
    ]
    return f"<GroupHeader><Section SectionNumber=\"0\">{''.join(gh_fields)}</Section></GroupHeader>"


def make_detail():
    det_fields = [
        make_field("Field22", "{lstDiarioVentas.PrecioImp}", fmt(rflt(50,10000)), raw(rflt(50,10000))),
        make_field("Field23", "{lstDiarioVentas.Cantidad}", fmt(rflt(1,999)), raw(rflt(1,999))),
        make_field("Field38", "{lstDiarioVentas.Descripcion}", rand_desc(), rand_desc()),
        make_field("Field39", "{lstDiarioVentas.IdArticulo}", rand_art(), rand_art()),
        make_field("Field61", "{lstDiarioVentas.ValorImp}", fmt(rflt(10,10000)), raw(rflt(10,10000))),
        make_field("Field73", "{lstDiarioVentas.PorcDesc}", fmt(rflt(0,30)), raw(rflt(0,30))),
    ]
    extra = ""
    if random.random() < 0.3:
        v = rflt(0, 30)
        extra += make_field("FieldG", "{lstDiarioVentas.PorcDescG}", fmt(v), raw(v))
    if random.random() < 0.7:
        extra += '<Text Name="Text20"><TextValue>%</TextValue></Text>'
    return f"<Details Level=\"3\"><Section SectionNumber=\"0\">{''.join(det_fields)}{extra}</Section></Details>"


def make_group_block(min_det=1, max_det=12):
    n_det = random.randint(min_det, max_det)
    details = "".join(make_detail() for _ in range(n_det))
    return f"<Group Level=\"2\">{make_group_header()}{details}</Group>"


def generate_file(target_mb: int, path: Path):
    target = target_mb * 1024 * 1024
    head_bytes = HEAD.encode("utf-8")
    tail_bytes = TAIL.encode("utf-8")

    with open(path, "wb") as f:
        f.write(head_bytes)
        written = len(head_bytes)
        count = 0

        while written < target - len(tail_bytes) - 50000:
            block = make_group_block().encode("utf-8")
            f.write(block)
            written += len(block)
            count += 1
            if count % 200 == 0:
                mb = written / 1024 / 1024
                print(f"  Groups: {count}, ~{mb:.1f} MB", end="\r")

        f.write(tail_bytes)

    actual = os.path.getsize(path) / 1024 / 1024
    print(f"\n  Done: {path.name} — {actual:.1f} MB, {count} invoice groups")
    return actual


def bench_native_speed(path, label, fn, **kwargs):
    t0 = time.perf_counter()
    tbl = fn(path, **kwargs)
    t1 = time.perf_counter()
    dur = t1 - t0
    rows = tbl.num_rows
    size = os.path.getsize(path)
    print(f"  {label:30s}  {rows:>7,} rows  {dur:.4f}s  {rows/dur:>8,.0f} rows/s  {size/dur/1024/1024:>6.1f} MB/s")


def bench_source_speed(path, label, engine):
    from crxml import CrystalXMLSource
    src = CrystalXMLSource(path, row_tag="Details", engine=engine)
    t0 = time.perf_counter()
    n = sum(1 for _ in src)
    t1 = time.perf_counter()
    dur = t1 - t0
    size = os.path.getsize(path)
    print(f"  {label:30s}  {n:>7,} rows  {dur:.4f}s  {n/dur:>8,.0f} rows/s  {size/dur/1024/1024:>6.1f} MB/s")


def bench_source_arrow(path, label, engine):
    from crxml import CrystalXMLSource

    src = CrystalXMLSource(path, row_tag="Details", engine=engine)
    t0 = time.perf_counter()
    tbl = src.to_arrow()
    t1 = time.perf_counter()
    dur = t1 - t0
    size = os.path.getsize(path)
    print(f"  {label:30s}  {tbl.num_rows:>7,} rows  {dur:.4f}s  {tbl.num_rows/dur:>8,.0f} rows/s  {size/dur/1024/1024:>6.1f} MB/s")


def bench_source_dataframe(path, label, engine):
    from crxml import CrystalXMLSource

    src = CrystalXMLSource(path, row_tag="Details", engine=engine)
    t0 = time.perf_counter()
    df = src.to_dataframe()
    t1 = time.perf_counter()
    dur = t1 - t0
    size = os.path.getsize(path)
    print(f"  {label:30s}  {len(df):>7,} rows  {dur:.4f}s  {len(df)/dur:>8,.0f} rows/s  {size/dur/1024/1024:>6.1f} MB/s")


def bench_native_mem(path, label, fn, **kwargs):
    import tracemalloc

    tracemalloc.start()
    t0 = time.perf_counter()
    tbl = fn(path, **kwargs)
    t1 = time.perf_counter()
    _, peak = tracemalloc.get_traced_memory()
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024 if resource else 0.0
    print(f"  {label:30s}  {tbl.num_rows:>7,} rows  {t1 - t0:.3f}s  {peak / 1024 / 1024:>5.1f} MB py  {rss:>5.1f} MB rss")


def bench_source_mem(path, label, engine):
    import tracemalloc

    from crxml import CrystalXMLSource

    tracemalloc.start()
    t0 = time.perf_counter()
    n = sum(1 for _ in CrystalXMLSource(path, row_tag="Details", engine=engine))
    t1 = time.perf_counter()
    _, peak = tracemalloc.get_traced_memory()
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024 if resource else 0.0
    print(f"  {label:30s}  {n:>7,} rows  {t1 - t0:.3f}s  {peak / 1024 / 1024:>5.1f} MB py  {rss:>5.1f} MB rss")


def bench_source_dataframe_mem(path, label, engine):
    import tracemalloc

    from crxml import CrystalXMLSource

    tracemalloc.start()
    t0 = time.perf_counter()
    df = CrystalXMLSource(path, row_tag="Details", engine=engine).to_dataframe()
    t1 = time.perf_counter()
    _, peak = tracemalloc.get_traced_memory()
    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024 if resource else 0.0
    print(f"  {label:30s}  {len(df):>7,} rows  {t1 - t0:.3f}s  {peak / 1024 / 1024:>5.1f} MB py  {rss:>5.1f} MB rss")


def run():
    import argparse
    parser = argparse.ArgumentParser(description="crxml benchmark suite")
    parser.add_argument("--gen-only", action="store_true", help="Only generate files")
    args = parser.parse_args()

    targets = [(10, OUT_DIR / "test_10mb.xml"),
               (50, OUT_DIR / "test_50mb.xml"),
               (100, OUT_DIR / "test_100mb.xml")]

    print("=" * 60)
    print("Generating synthetic CR XML files...")
    print("=" * 60)

    for mb, p in targets:
        if p.exists():
            print(f"\nSkipping {p.name} ({p.stat().st_size/1024/1024:.1f} MB)")
            continue
        print(f"\nGenerating {mb} MB file...")
        generate_file(mb, p)

    if args.gen_only:
        return

    bench_path = targets[-1][1]
    engines = ["stream", "columnar", "parallel"]

    print("\n" + "=" * 60)
    print("Native Export Benchmarks (100 MB)")
    print("=" * 60)
    native = [
        ("read_to_columnar", lambda path: _core.read_to_columnar(path, row_tag="Details")),
        ("read_to_columnar_multi", lambda path: _core.read_to_columnar_multi(path, row_tag="Details", num_chunks=2)),
        ("read_to_columnar_par", lambda path: _core.read_to_columnar_par(path, row_tag="Details", num_chunks=4)),
    ]
    for name, fn in native:
        print(f"\n--- {name} ---")
        bench_native_speed(str(bench_path), name, fn)

    print("\n" + "=" * 60)
    print("Source Row-Iteration Benchmarks (100 MB)")
    print("=" * 60)
    for engine in engines:
        print(f"\n--- {engine} ---")
        bench_source_speed(str(bench_path), f"Iter 100 MB [{engine}]", engine)

    print("\n" + "=" * 60)
    print("Source Arrow Benchmarks (100 MB)")
    print("=" * 60)
    for engine in engines:
        print(f"\n--- {engine} ---")
        bench_source_arrow(str(bench_path), f"Arrow 100 MB [{engine}]", engine)

    print("\n" + "=" * 60)
    print("Source DataFrame Benchmarks (100 MB)")
    print("=" * 60)
    for engine in engines:
        print(f"\n--- {engine} ---")
        bench_source_dataframe(str(bench_path), f"DataFrame 100 MB [{engine}]", engine)

    print("\n" + "=" * 60)
    print("Native Export Memory (100 MB)")
    print("=" * 60)
    for name, fn in native:
        print(f"\n--- {name} ---")
        bench_native_mem(str(bench_path), name, fn)

    print("\n" + "=" * 60)
    print("Source Row-Iteration Memory (100 MB)")
    print("=" * 60)
    for engine in engines:
        print(f"\n--- {engine} ---")
        bench_source_mem(str(bench_path), f"Iter 100 MB [{engine}]", engine)

    print("\n" + "=" * 60)
    print("Source DataFrame Memory (100 MB)")
    print("=" * 60)
    for engine in engines:
        print(f"\n--- {engine} ---")
        bench_source_dataframe_mem(str(bench_path), f"DataFrame 100 MB [{engine}]", engine)


if __name__ == "__main__":
    run()
