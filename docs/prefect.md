# Prefect Integration

Use crxml inside Prefect 2.x flows to parse Crystal Reports XML files as
part of a larger data pipeline.

## Installation

```bash
pip install crxml prefect
```

## Basic flow

```python
from pathlib import Path
from prefect import flow, task
from crxml import CrystalXMLSource, RenameFields, CastTypes, to_csv


@task
def parse_report(path: str) -> list[dict]:
    src = CrystalXMLSource(path)
    return list(src)


@task
def transform_rows(rows: list[dict]) -> list[dict]:
    pipe = (
        rows
        | RenameFields({
            "{Report.InvoiceNo}": "invoice",
            "{Report.Customer}": "customer",
            "{Report.Amount}": "amount",
        })
        | CastTypes({"amount": float})
    )
    return list(pipe)


@task
def export_csv(rows: list[dict], output_path: str) -> None:
    to_csv(rows, output_path)


@flow
def crxml_pipeline(input_path: str, output_path: str = "output.csv"):
    rows = parse_report(input_path)
    cleaned = transform_rows(rows)
    export_csv(cleaned, output_path)


if __name__ == "__main__":
    crxml_pipeline("report.xml")
```

## Streaming flow with task runners

For large files, use Prefect's `ThreadPoolTaskRunner` to stream rows without
loading everything into memory:

```python
from prefect import flow, task
from prefect.task_runners import ThreadPoolTaskRunner
from crxml import CrystalXMLSource, RenameFields, CastTypes, to_csv


@task
def stream_to_csv(input_path: str, output_path: str) -> None:
    pipe = (
        CrystalXMLSource(input_path)
        | RenameFields({
            "{Report.InvoiceNo}": "invoice",
            "{Report.Amount}": "amount",
        })
        | CastTypes({"amount": float})
    )
    to_csv(pipe, output_path)


@flow(task_runner=ThreadPoolTaskRunner())
def crxml_streaming_flow(input_path: str):
    stream_to_csv(input_path, "output.csv")
```

The file is streamed row by row. RSS stays constant regardless of file size.

## Parallel parsing with mapped tasks

Use Prefect task mapping to parse multiple files in parallel:

```python
from pathlib import Path
from prefect import flow, task
from crxml import CrystalXMLSource, collect


@task
def parse_single(path: str) -> dict:
    rows = collect(CrystalXMLSource(path))
    return {"file": path, "rows": len(rows)}


@flow
def parse_directory(data_dir: str = "./data"):
    paths = [str(p) for p in Path(data_dir).glob("*.xml")]
    results = parse_single.map(paths)
    for r in results:
        print(f"{r['file']}: {r['rows']} rows")
```

## Error handling and retries

```python
from prefect import flow, task
from crxml import CrystalXMLSource, collect


@task(retries=2, retry_delay_seconds=5)
def parse_with_retry(path: str) -> list[dict]:
    return collect(CrystalXMLSource(path))


@flow
def resilient_parse(path: str):
    try:
        rows = parse_with_retry(path)
        print(f"Parsed {len(rows)} rows")
    except FileNotFoundError:
        print(f"File not found: {path}")
    except ValueError as e:
        print(f"Parse error: {e}")
```

## Caching parsed results

```python
from pathlib import Path
from prefect import flow, task
from crxml import CrystalXMLSource, collect


@task(cache_policy=INPUTS)
def parse_cached(path: str) -> list[dict]:
    return collect(CrystalXMLSource(path))


@flow
def cached_parse_flow(path: str):
    rows = parse_cached(path)
    print(f"Parsed {len(rows)} rows")
    return rows
```

Prefect caches the task result keyed on the input path. Re-running with the
same file skips parsing.

## Deployment notes

- crxml's Rust parser releases the GIL during XML processing, so it does not
  block the asyncio event loop in Prefect's async task runner.
- Each task creates its own `CrystalXMLSource` instance. There is no shared
  state between tasks.
- For very large files (over 500 MB), the streaming approach is strongly
  recommended over `collect()` to avoid memory pressure.
- Prefect's `ProcessPoolTaskRunner` is not needed because crxml has its own
  parallel mode via `Pipeline.parallel()`.
