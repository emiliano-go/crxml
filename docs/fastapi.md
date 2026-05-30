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
