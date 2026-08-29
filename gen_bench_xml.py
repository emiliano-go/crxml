#!/usr/bin/env python3
"""Generate synthetic Crystal Report XML for predicate-first benchmarks.

Output: test_533mb.xml (~533 MB, 11 fields per row)

Format per row (must match scanner expectations):
  <Details Level="N">
  <Field Name="FieldK" FieldName="{a.ColK}"><FormattedValue>V</FormattedValue><Value>V</Value></Field>
  ...
  <Section SectionNumber="0"/>
  </Details>

Field6 is the predicate column: ~6% of rows have value "01-00123" (matching),
the rest have unique values (non-matching).
"""

import os
import sys

TARGET_BYTES = 533 * 1024 * 1024  # 533 MB
N_FIELDS = 11
PREDICATE_FIELD = 6  # 1-indexed: Field6 (middle)
PREDICATE_MATCH_VALUE = "01-00123"
SELECTIVITY = 0.06  # ~6% of rows match

# Static values for non-predicate fields (varied enough for realism)
STATIC_VALUES = {
    1: ("Distribuidora del Sur S.A.", "Distribuidora del Sur S.A."),
    2: ("1234.56", "1234.56"),
    3: ("ART-001-LG", "ART-001-LG"),
    4: ("2024-01-15", "2024-01-15"),
    5: ("Cordoba", "Cordoba"),
    7: ("URGENTE", "URGENTE"),
    8: ("15000.00", "15000.00"),
    9: ("PENDIENTE", "PENDIENTE"),
    10: ("Buenos Aires 1234", "Buenos Aires 1234"),
    11: ("IVA 21%", "IVA 21%"),
}

FIELD_NAMES = [f"Field{k}" for k in range(1, N_FIELDS + 1)]
FIELD_ATTRS = [f"{{a.Col{k}}}" for k in range(1, N_FIELDS + 1)]


def build_row(i: int) -> bytes:
    """Build one <Details> row."""
    parts = [b'<Details Level="']
    parts.append(str(i % 5).encode())
    parts.append(b'">\n')

    for k in range(1, N_FIELDS + 1):
        name = FIELD_NAMES[k - 1]
        attr = FIELD_ATTRS[k - 1]

        if k == PREDICATE_FIELD:
            # Predicate column: ~6% match "01-00123", rest get varied values
            if i % int(1.0 / SELECTIVITY) == 0:
                val = PREDICATE_MATCH_VALUE
            else:
                val = f"CUST-{i:07d}"
        else:
            fv, v = STATIC_VALUES[k]
            val = fv

        parts.append(b'<Field Name="')
        parts.append(name.encode())
        parts.append(b'" FieldName="')
        parts.append(attr.encode())
        parts.append(b'"><FormattedValue>')
        parts.append(val.encode())
        parts.append(b'</FormattedValue><Value>')
        parts.append(val.encode())
        parts.append(b'</Value></Field>\n')

    parts.append(b'<Section SectionNumber="0"/>\n')
    parts.append(b'</Details>\n')
    return b''.join(parts)


def main():
    out_path = os.path.join(os.path.dirname(__file__), "test_533mb.xml")

    print(f"Generating ~533 MB CR XML with {N_FIELDS} fields/row...")
    print(f"  Predicate column: Field{PREDICATE_FIELD} (~{SELECTIVITY*100:.0f}% selectivity)")
    print(f"  Match value: '{PREDICATE_MATCH_VALUE}'")

    # Estimate rows needed: build one row, measure its size, extrapolate
    sample_row = build_row(0)
    row_bytes = len(sample_row)
    n_rows = TARGET_BYTES // row_bytes
    print(f"  Estimated row size: {row_bytes} bytes")
    print(f"  Target rows: {n_rows:,}")

    header = b'<?xml version="1.0" encoding="UTF-8"?><CrystalReport>'
    footer = b'</CrystalReport>'
    total_est = len(header) + n_rows * row_bytes + len(footer)
    print(f"  Estimated total: {total_est / 1024 / 1024:.1f} MB")

    with open(out_path, 'wb') as f:
        f.write(header)
        for i in range(n_rows):
            f.write(build_row(i))
            if (i + 1) % 100_000 == 0:
                print(f"  ... {i+1:,} rows written", file=sys.stderr)
        f.write(footer)

    actual = os.path.getsize(out_path)
    print(f"\nDone: {out_path}")
    print(f"  Actual size: {actual / 1024 / 1024:.1f} MB")
    print(f"  Rows: {n_rows:,}")


if __name__ == "__main__":
    main()
