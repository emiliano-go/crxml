# Troubleshooting

## Common errors

### FileNotFoundError

```python
CrystalXMLSource("nonexistent.xml")
# FileNotFoundError: File not found: nonexistent.xml
```

The path must be a local file path. Remote URLs and file-like objects are not
supported.

### Empty result (zero rows)

The most common cause is a wrong `row_tag`. CR XML files use different tag
names for record rows. Inspect the file to find the correct tag:

```bash
grep -o '<[A-Za-z][A-Za-z0-9]*' report.xml | sort | uniq -c | sort -rn | head -10
```

The most frequent non-wrapper tag is usually the row tag. Pass it explicitly:

```python
src = CrystalXMLSource("report.xml", row_tag="Detail")
```

### ValueError on bad CR XML

The parser raises `ValueError` if the XML is malformed or does not match the
Crystal Reports schema. Check that:

- The file is well-formed XML (validate with `xmllint`)
- The file contains repeating row elements (not a single record)
- The row tag contains `<Field>` children with `FieldName` attributes

### TypeError from CastTypes

```python
CastTypes({"amount": float}, errors="raise")  # raises on uncastable values
```

Use `errors="coerce"` to replace uncastable values with `None`, or
`errors="skip"` to leave them unchanged.

### UnpicklableStageError

Raised when calling `.parallel()` on a pipeline with non-picklable stages.

Common causes:

- Lambdas used as predicates in `FilterRows`
- Local/nested functions used as stages
- Custom class instances that cannot be pickled

Fix: use module-level functions or built-in stages with the keyword-based
`FilterRows(field=..., op=..., value=...)` API.

```python
# Causes UnpicklableStageError:
pipe | FilterRows(lambda r: r.get("x") == "y")

# Works with .parallel():
pipe | FilterRows(field="x", op="==", value="y")
```

## FAQ

### How do I find the right row_tag?

Open the XML in a text editor. Look for a repeating element that wraps each
record. In standard CR XML this is `<Details>`, but it varies by report.
Common values: `Details`, `Detail`, `Row`, `Record`, `Item`, `Group`.

### Why are my field names like {Report.InvoiceNo}?

Crystal Reports XML uses `{Report.FieldName}` as the `FieldName` attribute.
This is the raw key from the XML. Use `RenameFields` to map them to shorter
names:

```python
CrystalXMLSource("report.xml") | RenameFields({
    "{Report.InvoiceNo}": "invoice",
    "{Report.Customer}": "customer",
})
```

### Parallel mode is slower than sequential

Parallel mode adds overhead for batch serialization and IPC. It is
recommended for files larger than 50 MB. For small files, sequential
iteration is faster.

If parallel is slower on a large file, check:

- Are all stages picklable? (`validate_stages_picklable` from `crxml.parallel`)
- Is the file on a fast SSD? (disk I/O can be the bottleneck)
- Is `batch_size` tuned? (try 5000 to 20000)
- Are you using the right number of workers? (defaults to CPU count)

### Pipeline fusion is not happening

Check that your stages are compatible:

- `RenameFields`, `CastTypes`, `DropFields`, `FilterRows` with keyword args
  all support columnar fusion.
- Custom stages need `_plan_kwargs()` returning a dict.
- Non-fusable stages (lambdas, generators) break the fused chain. Put them
  after fused stages to minimize the performance impact.

### The Rust extension won't build

Ensure you have the Rust toolchain installed:

```bash
rustup install stable
```

If building with the `columnar` feature, additional dependencies may be
required (Arrow, Parquet). On Linux, install:

```bash
sudo apt-get install libarrow-dev libparquet-dev
```

For other platforms, see the Apache Arrow C++ install guide.

### How do I process files larger than RAM?

crxml streams the XML in constant memory. The RSS stays well below the file
size (about 75 MB for a 100 MB file). Use `to_csv` or chunked `to_dataframe`
to avoid loading all rows at once:

```python
to_csv(pipe, "output.csv")
```
