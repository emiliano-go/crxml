import csv
import warnings
from pathlib import Path
from typing import Iterable

def to_dataframe(
    pipeline: Iterable[dict],
    chunksize: int | None = None,
    dtype_backend: str = "pyarrow",
) -> "pd.DataFrame":
    import pandas as pd
    types_mapper = pd.ArrowDtype if dtype_backend == "pyarrow" else None
    if chunksize is None:
        if hasattr(pipeline, "_to_arrow"):
            table = pipeline._to_arrow()
            if table is not None:
                return table.to_pandas(types_mapper=types_mapper)
        if hasattr(pipeline, "_iter_batches"):
            chunks = [pd.DataFrame.from_records(batch) for batch in pipeline._iter_batches()]
            return pd.concat(chunks, ignore_index=True) if chunks else pd.DataFrame()
        return pd.DataFrame.from_records(iter(pipeline))
    chunks = []
    batch = []
    for rec in pipeline:
        batch.append(rec)
        if len(batch) >= chunksize:
            chunks.append(pd.DataFrame.from_records(batch))
            batch = []
    if batch:
        chunks.append(pd.DataFrame.from_records(batch))
    return pd.concat(chunks, ignore_index=True) if chunks else pd.DataFrame()

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
