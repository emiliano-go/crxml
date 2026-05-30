# Branching

Pipelines are immutable. This makes it safe to reuse a base pipeline as the
starting point for multiple branches.

## Example: split by region

```python
from crxml import CrystalXMLSource, FilterRows, to_csv

base = CrystalXMLSource("report.xml")

north = base | FilterRows(lambda r: r.get("region") == "north")
south = base | FilterRows(lambda r: r.get("region") == "south")

to_csv(north, "north.csv")
to_csv(south, "south.csv")
```

Each branch is independent. The source file is re-read for each branch, so
disk I/O scales linearly with the number of branches.

## Example: different transformations per branch

```python
from crxml import CrystalXMLSource, RenameFields, CastTypes, to_dataframe, to_csv

base = CrystalXMLSource("report.xml")

# Branch A: rename + CSV export
branch_a = base | RenameFields({"f1": "invoice", "f2": "amount"})
to_csv(branch_a, "export.csv")

# Branch B: cast + DataFrame
branch_b = base | CastTypes({"amount": float})
df = to_dataframe(branch_b)
```

## Performance note

Each branch re-opens and re-parses the source file. For large files with
many branches, consider:

- Using a single pipeline with `FilterRows` for each output path
- Pre-filtering with external tools like `grep` or `xsv`
- Caching the parsed stream to a temporary file first
