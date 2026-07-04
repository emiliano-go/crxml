import csv
from pathlib import Path
from typing import Iterable

def to_dataframe(pipeline: Iterable[dict], chunksize: int | None = None) -> "pd.DataFrame":
    import pandas as pd
    if chunksize is None:
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
) -> None:
    path = Path(path)
    stream = iter(pipeline)
    try:
        first = next(stream)
    except StopIteration:
        with open(path, "w", encoding=encoding) as f:
            pass
        return
    with open(path, "w", encoding=encoding, newline="") as f:
        writer = csv.DictWriter(
            f, fieldnames=[*first], delimiter=delimiter,
            extrasaction='ignore'
        )
        writer.writeheader()
        writer.writerow(first)
        for rec in stream:
            writer.writerow(rec)

def collect(pipeline: Iterable[dict]) -> list[dict]:
    if hasattr(pipeline, "_iter_batches"):
        rows = []
        for batch in pipeline._iter_batches():
            rows.extend(batch)
        return rows
    return list(pipeline)
