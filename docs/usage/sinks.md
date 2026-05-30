# Sinks

Sinks terminate a pipeline and materialize the result.

## to_dataframe

```python
to_dataframe(pipeline, chunksize: int | None = None) -> pd.DataFrame
```

Collects all rows into a pandas DataFrame.

- `chunksize=None` (default): builds a list of dicts, then constructs the
  DataFrame. Simple but memory-intensive for large outputs.
- `chunksize=N`: incrementally builds the DataFrame in chunks of N rows,
  then concatenates. Lower peak memory.

```python
from crxml import CrystalXMLSource, RenameFields, to_dataframe

pipe = CrystalXMLSource("report.xml") | RenameFields({"f1": "name"})
df = to_dataframe(pipe, chunksize=10000)
```

## to_csv

```python
to_csv(pipeline, path: str | Path, **csv_writer_kwargs) -> None
```

Streams rows directly to CSV. Supports all `csv.writer` kwargs via
`**csv_writer_kwargs`:

```python
from crxml import CrystalXMLSource, to_csv

pipe = CrystalXMLSource("report.xml")
to_csv(pipe, "output.csv", delimiter=";", quoting=1)
```

The CSV is written incrementally as rows are produced. No intermediate list.

## collect

```python
collect(pipeline) -> list[dict]
```

Materializes the pipeline into a list of dicts. Useful for testing and
debugging:

```python
from crxml import CrystalXMLSource, collect

rows = collect(CrystalXMLSource("report.xml"))
assert len(rows) == 8306
```

## XLSX via openpyxl

crxml does not have a built-in XLSX sink, but you can build one easily:

```python
from openpyxl import Workbook
from crxml import CrystalXMLSource, collect

rows = collect(CrystalXMLSource("report.xml"))

wb = Workbook()
ws = wb.active
if rows:
    ws.append(list(rows[0].keys()))
    for row in rows:
        ws.append(list(row.values()))
wb.save("output.xlsx")
```

For large files, chunk the writes to avoid loading all rows into memory.
