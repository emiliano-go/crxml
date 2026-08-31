#!/usr/bin/env python3
"""Generate anonymized synthetic Crystal Reports XML benchmark files.

The source XML is inspected only for structural shape. No source text is
copied: report metadata, field names, attributes, values, and row contents are
discarded. The output uses generic names and deterministic neutral values.

Example::

    python scripts/generate_synthetic_benchmark.py \
        --source ~/Downloads/base-data.xml \
        --output bench_data/synthetic_533mb.xml \
        --target-mib 533
"""

from __future__ import annotations

import argparse
import random
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Shape:
    row_tag: str
    attribute_count: int
    field_count: int
    text_count: int
    section_count: int


def local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1]


def inspect_shape(source: Path) -> Shape:
    """Read only enough XML structure to describe one representative row."""
    for event, elem in ET.iterparse(source, events=("end",)):
        tag = local_name(elem.tag)
        if tag not in {"Row", "Details"}:
            continue
        descendants = [local_name(child.tag) for child in elem.iter() if child is not elem]
        return Shape(
            row_tag=tag,
            attribute_count=len(elem.attrib),
            field_count=descendants.count("Field"),
            text_count=descendants.count("Text"),
            section_count=descendants.count("Section"),
        )
    raise ValueError(f"no Row or Details element found in {source}")


def xml_escape(value: str) -> str:
    return (
        value.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def synthetic_value(index: int, field: int) -> str:
    """Return neutral data with realistic mixed text lengths."""
    if field % 5 == 0:
        return f"{(index * 17 + field) % 100000}.25"
    if field % 5 == 1:
        return f"ITEM-{(index * 31 + field) % 100000:05d}"
    if field % 5 == 2:
        return f"Synthetic description {(index + field) % 97:02d}"
    if field % 5 == 3:
        return f"{(index * 7 + field) % 1000}"
    return f"GROUP-{(index + field) % 23:02d}"


def build_row(index: int, shape: Shape, rng: random.Random) -> bytes:
    parts = [f'<{shape.row_tag}']
    for attr in range(shape.attribute_count):
        parts.append(f' Attr{attr + 1}="{(index + attr) % 11}"')
    parts.append(">")

    for field in range(1, shape.field_count + 1):
        # Preserve a modest sparse-column pattern without source values.
        if field == shape.field_count and index % 20 >= 16:
            continue
        value = xml_escape(synthetic_value(index, field))
        parts.append(
            f'<Field Name="Field{field}" FieldName="{{synthetic.Field{field}}}">'
            f"<FormattedValue>{value}</FormattedValue>"
            f"<Value>{value}</Value></Field>"
        )

    for text in range(1, shape.text_count + 1):
        parts.append(f'<Text Name="Text{text}"><TextValue>TEXT-{index % 31:02d}</TextValue></Text>')

    for _ in range(shape.section_count):
        parts.append(f'<Section SectionNumber="{index % 4}"/>')

    parts.append(f"</{shape.row_tag}>")
    return "".join(parts).encode("utf-8")


def generate(source: Path, output: Path, target_mib: int, seed: int) -> None:
    if target_mib <= 0:
        raise ValueError("target-mib must be positive")
    shape = inspect_shape(source)
    output.parent.mkdir(parents=True, exist_ok=True)
    rng = random.Random(seed)
    header = f'<?xml version="1.0" encoding="UTF-8"?><SyntheticReport>' .encode("utf-8")
    footer = b"</SyntheticReport>"
    target = target_mib * 1024 * 1024
    with output.open("wb", buffering=1024 * 1024) as stream:
        stream.write(header)
        rows = 0
        while stream.tell() < target - len(footer):
            index = rows
            stream.write(build_row(index, shape, rng))
            rows += 1
        stream.write(footer)

    size = output.stat().st_size
    print(f"source structure: row_tag={shape.row_tag}, attrs={shape.attribute_count}, "
          f"fields={shape.field_count}, texts={shape.text_count}, sections={shape.section_count}")
    print(f"generated: {output} ({size / 1024 / 1024:.1f} MiB, {rows:,} rows)")
    print("data policy: source values and identifiers were not copied")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--target-mib", type=int, required=True)
    parser.add_argument("--seed", type=int, default=20260830)
    args = parser.parse_args()
    generate(args.source.expanduser(), args.output, args.target_mib, args.seed)


if __name__ == "__main__":
    main()
