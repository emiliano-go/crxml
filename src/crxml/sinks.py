import csv
import warnings
from pathlib import Path
from typing import Iterable

def to_csv(
    pipeline: Iterable[dict],
    path: str | Path,
    encoding: str = "utf-8",
    delimiter: str = ",",
    fieldnames: list[str] | None = None,
) -> None:
    """Stream records to CSV.

    The header comes from ``fieldnames`` if given, else from the first
    record.  CR exports are ragged: rows may carry fields the header does
    not know about.  Such fields are omitted from the output and a
    ``UserWarning`` names them (once per field) instead of dropping them
    silently; pass an explicit ``fieldnames`` union to include them.
    Fields missing from a record are written as empty strings.
    """
    path = Path(path)
    stream = iter(pipeline)
    try:
        first = next(stream)
    except StopIteration:
        with open(path, "w", encoding=encoding) as f:
            pass
        return
    if fieldnames is None:
        fieldnames = [*first]
    known = set(fieldnames)
    warned: set[str] = set()
    with open(path, "w", encoding=encoding, newline="") as f:
        writer = csv.DictWriter(
            f, fieldnames=fieldnames, delimiter=delimiter,
            extrasaction='ignore'
        )
        writer.writeheader()
        writer.writerow(first)
        for record in stream:
            fresh = {k for k in record if k not in known} - warned
            if fresh:
                warned |= fresh
                warnings.warn(
                    f"to_csv: field(s) {sorted(fresh)!r} are not in the CSV "
                    f"header and will be omitted; pass fieldnames= to "
                    f"include them",
                    UserWarning,
                    stacklevel=2,
                )
            writer.writerow(record)

def collect(pipeline: Iterable[dict]) -> list[dict]:
    """Materialize a pipeline into a plain list of dicts.

    Optimizes for columnar pipelines by converting via Arrow when possible,
    otherwise iterates the pipeline into a list.
    """
    if hasattr(pipeline, "_to_arrow"):
        table = pipeline._to_arrow()
        if table is not None:
            return table.to_pylist()
    if hasattr(pipeline, "_iter_batches"):
        rows = []
        for batch in pipeline._iter_batches():
            rows.extend(batch)
        return rows
    return list(pipeline)


def to_arrow(pipeline: Iterable[dict]):
    """Return a ``pyarrow.Table`` from a pipeline or source."""
    if hasattr(pipeline, "_to_arrow"):
        table = pipeline._to_arrow()
        if table is not None:
            return table
    if hasattr(pipeline, "to_arrow"):
        return pipeline.to_arrow()
    import pyarrow as pa
    return pa.Table.from_pylist(list(pipeline))


def to_pandas(pipeline: Iterable[dict], chunksize: int | None = None, dtype_backend: str = "pyarrow", memory=None, **kwargs):
    """Return a pandas DataFrame from a pipeline or source."""
    import pandas as pd
    types_mapper = pd.ArrowDtype if dtype_backend == "pyarrow" else None
    if memory is not None and hasattr(pipeline, "iter_record_batches"):
        chunks = []
        for batch in pipeline.iter_record_batches(memory=memory, **kwargs):
            chunks.append(batch.to_pandas(types_mapper=types_mapper))
        return pd.concat(chunks, ignore_index=True) if chunks else pd.DataFrame()
    table = to_arrow(pipeline)
    if chunksize is not None:
        chunks = []
        for batch in table.to_batches(max_chunksize=chunksize):
            chunks.append(batch.to_pandas(types_mapper=types_mapper))
        return pd.concat(chunks, ignore_index=True) if chunks else pd.DataFrame()
    if dtype_backend == "pyarrow":
        return table.to_pandas(types_mapper=types_mapper)
    return table.to_pandas()


def to_polars(pipeline: Iterable[dict], memory=None, **kwargs):
    """Return a Polars DataFrame from a pipeline or source."""
    import polars as pl
    if memory is not None and hasattr(pipeline, "iter_record_batches"):
        chunks = []
        for batch in pipeline.iter_record_batches(memory=memory, **kwargs):
            chunks.append(pl.from_arrow(batch))
        return pl.concat(chunks) if chunks else pl.DataFrame()
    return pl.from_arrow(to_arrow(pipeline))


def to_parquet(pipeline: Iterable[dict], path: str | Path, memory=None, **kwargs):
    """Write a pipeline or source to Parquet."""
    import pyarrow.parquet as pq
    if memory is not None and hasattr(pipeline, "iter_record_batches"):
        parquet_keys = {"compression", "compression_level", "row_group_size", "use_dictionary", "write_statistics"}
        parquet_kwargs = {k: v for k, v in kwargs.items() if k in parquet_keys}
        iter_kwargs = {k: v for k, v in kwargs.items() if k not in parquet_keys}
        writer = None
        for batch in pipeline.iter_record_batches(memory=memory, **iter_kwargs):
            if writer is None:
                writer = pq.ParquetWriter(str(path), batch.schema, **parquet_kwargs)
            writer.write_batch(batch)
        if writer is not None:
            writer.close()
        return
    pq.write_table(to_arrow(pipeline), str(path), **kwargs)


def to_dataframe(pipeline: Iterable[dict], chunksize: int | None = None, dtype_backend: str = "pyarrow"):
    """Alias for :func:`to_pandas`, kept for backward compatibility with crxml 2.1."""
    return to_pandas(pipeline, chunksize=chunksize, dtype_backend=dtype_backend)
