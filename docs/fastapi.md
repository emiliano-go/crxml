# FastAPI Integration

A production pattern for accepting CR XML file uploads, parsing server-side,
and returning structured data.

## Upload endpoint

```python
from fastapi import FastAPI, UploadFile, File, HTTPException
from tempfile import NamedTemporaryFile
from crxml import CrystalXMLSource, collect
import os

app = FastAPI()
MAX_FILE_SIZE = 500 * 1024 * 1024  # 500 MB

@app.post("/parse-report")
async def parse_report(file: UploadFile = File(...)):
    if file.size and file.size > MAX_FILE_SIZE:
        raise HTTPException(413, "File too large")

    ext = os.path.splitext(file.filename or "")[1].lower()
    if ext not in (".xml", ".rpt"):
        raise HTTPException(422, "Unsupported file type")

    with NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
        content = await file.read()
        if len(content) > MAX_FILE_SIZE:
            os.unlink(tmp.name)
            raise HTTPException(413, "File too large")
        tmp.write(content)
        tmp_path = tmp.name

    try:
        rows = collect(CrystalXMLSource(tmp_path))
        return {"rows": len(rows), "data": rows}
    except Exception as e:
        raise HTTPException(500, f"Parse failed: {e}")
    finally:
        os.unlink(tmp_path)
```

## Large file background processing

Offload parsing of large files to a background task and return a result ID:

```python
from fastapi import BackgroundTasks
from uuid import uuid4
from crxml import CrystalXMLSource, RenameFields, CastTypes, to_csv

results: dict[str, str] = {}


def process_large_file(tmp_path: str, result_id: str, output_path: str):
    pipe = (
        CrystalXMLSource(tmp_path)
        | RenameFields({
            "{Report.InvoiceNo}": "invoice",
            "{Report.Amount}": "amount",
        })
        | CastTypes({"amount": float})
    )
    to_csv(pipe, output_path)
    results[result_id] = output_path


@app.post("/parse-large")
async def parse_large(file: UploadFile = File(...), background: BackgroundTasks = BackgroundTasks()):
    with NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
        tmp.write(await file.read())
        tmp_path = tmp.name

    result_id = str(uuid4())
    output_path = f"/tmp/{result_id}.csv"
    background.add_task(process_large_file, tmp_path, result_id, output_path)

    return {"result_id": result_id, "status": "processing"}


@app.get("/results/{result_id}")
async def get_result(result_id: str):
    path = results.get(result_id)
    if path is None:
        raise HTTPException(404, "Result not found or still processing")
    return FileResponse(path, media_type="text/csv")
```

## Dependency injection for pipelines

Reusable pipeline factory via FastAPI dependencies:

```python
from fastapi import Depends
from crxml import CrystalXMLSource, RenameFields, CastTypes, collect

DEFAULT_MAPPING = {
    "{Report.InvoiceNo}": "invoice",
    "{Report.Customer}": "customer",
    "{Report.Amount}": "amount",
}


def get_pipeline(tmp_path: str, mapping: dict[str, str] | None = None):
    src = CrystalXMLSource(tmp_path)
    if mapping:
        src = src | RenameFields(mapping) | CastTypes({"amount": float})
    return src


@app.post("/parse-with-mapping")
async def parse_with_mapping(file: UploadFile = File(...)):
    with NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
        tmp.write(await file.read())
        tmp_path = tmp.name

    try:
        pipe = get_pipeline(tmp_path, DEFAULT_MAPPING)
        rows = collect(pipe)
        return {"rows": len(rows)}
    finally:
        os.unlink(tmp_path)
```

## Streaming CSV response

Stream CSV directly without buffering all rows:

```python
from fastapi.responses import StreamingResponse
import csv
import io


@app.post("/stream-csv")
async def stream_csv(file: UploadFile = File(...)):
    with NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
        tmp.write(await file.read())
        tmp_path = tmp.name

    async def row_generator():
        src = CrystalXMLSource(tmp_path)
        writer = None
        for row in src:
            if writer is None:
                output = io.StringIO()
                w = csv.DictWriter(output, fieldnames=list(row.keys()))
                w.writeheader()
                yield output.getvalue()
            output = io.StringIO()
            w = csv.DictWriter(output, fieldnames=list(row.keys()))
            w.writerow(row)
            yield output.getvalue()

    return StreamingResponse(
        row_generator(),
        media_type="text/csv",
        headers={"Content-Disposition": "attachment; filename=report.csv"}
    )
```

## Streaming XLSX response

```python
from fastapi.responses import StreamingResponse
from openpyxl import Workbook
from io import BytesIO

@app.post("/to-xlsx")
async def to_xlsx(file: UploadFile = File(...)):
    with NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
        tmp.write(await file.read())
        tmp_path = tmp.name

    try:
        rows = collect(CrystalXMLSource(tmp_path))
    finally:
        os.unlink(tmp_path)

    wb = Workbook()
    ws = wb.active
    if rows:
        ws.append(list(rows[0].keys()))
        for row in rows:
            ws.append(list(row.values()))

    buf = BytesIO()
    wb.save(buf)
    buf.seek(0)
    return StreamingResponse(buf, media_type="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet")
```

## Error handling

| Status | Condition                          |
|--------|------------------------------------|
| 413    | File exceeds size limit            |
| 422    | Unsupported file extension         |
| 500    | Parse failure (bad XML, CR format) |

## Thread safety

FastAPI runs route handlers in thread pool workers by default. The Rust parser
is `Send` but not `Sync`. Each request gets its own `CrystalXMLSource`
instance, so there is no shared state.
