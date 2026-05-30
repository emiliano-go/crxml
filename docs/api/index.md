## CrystalXMLSource

```python
CrystalXMLSource(source: str | Path | TextIOBase, row_tag: str = "Details")
```

| Param     | Type                          | Default      | Description                      |
|-----------|-------------------------------|--------------|----------------------------------|
| `source`  | `str \| Path \| TextIOBase`   | —            | Path or file-like object         |
| `row_tag` | `str`                         | `"Details"`  | XML tag for each record row      |

**Returns:** iterable of `dict[str, str]`

**Raises:** `FileNotFoundError`, `ValueError` (bad CR XML format)

## RenameFields

```python
RenameFields(mapping: dict[str, str])
```

| Param    | Type             | Description                        |
|----------|------------------|------------------------------------|
| `mapping`| `dict[str, str]` | Old key → new key mapping          |

**Fusable:** yes | **Picklable:** yes

## CastTypes

```python
CastTypes(types: dict[str, type], errors: str = "raise")
```

| Param   | Type             | Default   | Description                         |
|---------|------------------|-----------|-------------------------------------|
| `types` | `dict[str, type]`| —         | Field name → target type            |
| `errors`| `str`            | `"raise"` | One of `"raise"`, `"coerce"`, `"skip"` |

**Fusable:** yes | **Picklable:** yes

## DropFields

```python
DropFields(*fields: str)
```

| Param    | Type    | Description            |
|----------|---------|------------------------|
| `*fields`| `str`   | Keys to remove from rows |

**Fusable:** yes | **Picklable:** yes

## FilterRows

```python
FilterRows(predicate: Callable[[dict], bool])
```

| Param      | Type                       | Description                      |
|------------|----------------------------|----------------------------------|
| `predicate`| `Callable[[dict], bool]`   | Return `True` to keep the row    |

**Fusable:** yes | **Picklable:** no (unless a module-level function)

## Pipeline

```python
Pipeline(source: Iterable[dict], *stages: Stage)
```

Created implicitly via `|`. Not typically constructed directly.

### Methods

| Method      | Signature                               | Description                |
|-------------|-----------------------------------------|----------------------------|
| `__or__`    | `(self, stage) -> Pipeline`             | Append a stage             |
| `__iter__`  | `(self) -> Iterator[dict]`              | Iterate rows               |
| `parallel`  | `(self, workers=None, batch_size=10000)`| Return parallel variant    |

## to_dataframe

```python
to_dataframe(pipeline: Pipeline, chunksize: int | None = None) -> pd.DataFrame
```

| Param      | Type               | Default | Description                    |
|------------|--------------------|---------|--------------------------------|
| `pipeline` | `Pipeline`         | —       | Pipeline to consume            |
| `chunksize`| `int \| None`      | `None`  | Incremental chunk size         |

## to_csv

```python
to_csv(pipeline: Pipeline, path: str | Path, **csv_writer_kwargs) -> None
```

| Param               | Type               | Default | Description                  |
|---------------------|--------------------|---------|------------------------------|
| `pipeline`          | `Pipeline`         | —       | Pipeline to consume          |
| `path`              | `str \| Path`      | —       | Output CSV path              |
| `**csv_writer_kwargs`| `Any`             | —       | Forwarded to `csv.writer`    |

## collect

```python
collect(pipeline: Pipeline) -> list[dict]
```

| Param      | Type       | Description              |
|------------|------------|--------------------------|
| `pipeline` | `Pipeline` | Pipeline to materialize  |
