"""
Benchmark suite for crxml.
Generates 10 MB, 50 MB, and 100 MB synthetic Crystal Reports XML files
with random data, then measures throughput and memory usage.
"""

import os
import random
import time
import tracemalloc
import resource
import subprocess
import sys
from pathlib import Path
from xml.sax.saxutils import escape

HERE = Path(__file__).parent
OUT_DIR = HERE / "bench_data"
OUT_DIR.mkdir(exist_ok=True)

random.seed(42)

# ── Random data generators ───────────────────────────────────────────

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


# ── XML building blocks ───────────────────────────────────────────────

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
    """Generate <GroupHeader> for one invoice."""
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
    """Generate one <Details Level="3"> block."""
    det_fields = [
        make_field("Field22", "{lstDiarioVentas.PrecioImp}", fmt(rflt(50,10000)), raw(rflt(50,10000))),
        make_field("Field23", "{lstDiarioVentas.Cantidad}", fmt(rflt(1,999)), raw(rflt(1,999))),
        make_field("Field38", "{lstDiarioVentas.Descripcion}", rand_desc(), rand_desc()),
        make_field("Field39", "{lstDiarioVentas.IdArticulo}", rand_art(), rand_art()),
        make_field("Field61", "{lstDiarioVentas.ValorImp}", fmt(rflt(10,10000)), raw(rflt(10,10000))),
        make_field("Field73", "{lstDiarioVentas.PorcDesc}", fmt(rflt(0,30)), raw(rflt(0,30))),
    ]
    # Sometimes add extra fields and Text
    extra = ""
    if random.random() < 0.3:
        v = rflt(0, 30)
        extra += make_field("FieldG", "{lstDiarioVentas.PorcDescG}", fmt(v), raw(v))
    if random.random() < 0.7:
        extra += '<Text Name="Text20"><TextValue>%</TextValue></Text>'
    return f"<Details Level=\"3\"><Section SectionNumber=\"0\">{''.join(det_fields)}{extra}</Section></Details>"


def make_group_block(min_det=1, max_det=12):
    """Generate one <Group Level="2"> block (invoice + line items)."""
    n_det = random.randint(min_det, max_det)
    details = "".join(make_detail() for _ in range(n_det))
    return f"<Group Level=\"2\">{make_group_header()}{details}</Group>"


# ── File generation ────────────────────────────────────────────────────

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


# ── Benchmarks ─────────────────────────────────────────────────────────

def bench_speed(path, label):
    from crxml import CrystalXMLSource
    src = CrystalXMLSource(path, row_tag="Details")
    t0 = time.perf_counter()
    n = sum(1 for _ in src)
    t1 = time.perf_counter()
    dur = t1 - t0
    size = os.path.getsize(path)
    print(f"  {label:30s}  {n:>7,} rows  {dur:.4f}s  {n/dur:>8,.0f} rows/s  {size/dur/1024/1024:>6.1f} MB/s")


def bench_mem(path, label):
    code = f"""
import tracemalloc, resource
tracemalloc.start()
from crxml import CrystalXMLSource
n = sum(1 for _ in CrystalXMLSource("{path}", row_tag="Details"))
_, peak = tracemalloc.get_traced_memory()
rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024
print(f"ROWS={{n}} PY_PEAK={{peak}} RSS={{rss}}")
"""
    t0 = time.perf_counter()
    result = subprocess.run([sys.executable, "-c", code],
                            capture_output=True, text=True, timeout=120)
    t1 = time.perf_counter()
    dur = t1 - t0
    for line in result.stdout.splitlines():
        vals = {kv.split("=")[0]: kv.split("=")[1] for kv in line.split()}
        rows = int(vals["ROWS"])
        py_peak = int(vals["PY_PEAK"]) / 1024 / 1024
        rss = float(vals["RSS"])
        print(f"  {label:30s}  {rows:>7,} rows  {dur:.3f}s  {py_peak:>5.1f} MB py  {rss:>5.1f} MB rss")
        break


# ── Main ───────────────────────────────────────────────────────────────

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

    print("\n" + "=" * 60)
    print("Speed Benchmarks")
    print("=" * 60)
    for mb, p in targets:
        print(f"\n--- {mb} MB ---")
        bench_speed(str(p), f"Parse {mb} MB")

    print("\n" + "=" * 60)
    print("Memory Benchmarks")
    print("=" * 60)
    for mb, p in targets:
        print(f"\n--- {mb} MB ---")
        bench_mem(str(p), f"Stream {mb} MB")


if __name__ == "__main__":
    run()
