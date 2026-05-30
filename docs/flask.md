# Flask Integration

Use crxml inside Flask views to upload, parse, and return Crystal Reports
XML data as JSON, CSV, or XLSX.

## Upload + JSON response

```python
from flask import Flask, request, jsonify
from tempfile import NamedTemporaryFile
from crxml import CrystalXMLSource, collect
import os

app = Flask(__name__)
app.config["MAX_CONTENT_LENGTH"] = 500 * 1024 * 1024  # 500 MB


@app.post("/parse-report")
def parse_report():
    if "file" not in request.files:
        return jsonify({"error": "No file provided"}), 400

    file = request.files["file"]
    if not file.filename:
        return jsonify({"error": "Empty filename"}), 400

    ext = os.path.splitext(file.filename or "")[1].lower()
    if ext not in (".xml", ".rpt"):
        return jsonify({"error": "Unsupported file type"}), 422

    with NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
        tmp.write(file.read())
        tmp_path = tmp.name

    try:
        rows = collect(CrystalXMLSource(tmp_path))
        return jsonify({"rows": len(rows), "data": rows[:100]})
    except Exception as e:
        return jsonify({"error": str(e)}), 500
    finally:
        os.unlink(tmp_path)
```

## Streaming CSV download

```python
from flask import Response
import csv
import io


@app.post("/to-csv")
def to_csv():
    file = request.files["file"]

    with NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
        tmp.write(file.read())
        tmp_path = tmp.name

    def generate():
        src = CrystalXMLSource(tmp_path)
        writer = None
        for row in src:
            if writer is None:
                output = io.StringIO()
                writer = csv.DictWriter(output, fieldnames=list(row.keys()))
                writer.writeheader()
                yield output.getvalue()
            output = io.StringIO()
            writer = csv.DictWriter(output, fieldnames=list(row.keys()))
            writer.writerow(row)
            yield output.getvalue()

    return Response(generate(), mimetype="text/csv", headers={
        "Content-Disposition": "attachment; filename=report.csv"
    })
```

## XLSX download

```python
from flask import send_file
from openpyxl import Workbook
from io import BytesIO


@app.post("/to-xlsx")
def to_xlsx():
    file = request.files["file"]

    with NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
        tmp.write(file.read())
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

    return send_file(buf, mimetype="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                     as_attachment=True, download_name="report.xlsx")
```

## Error handling

| Status | Condition                          |
|--------|------------------------------------|
| 400    | No file or empty filename          |
| 413    | File exceeds size limit            |
| 422    | Unsupported file extension         |
| 500    | Parse failure (bad XML, CR format) |

## Thread safety

Flask's development server is single-threaded by default. In production with
a WSGI server (gunicorn, waitress), each worker has its own
`CrystalXMLSource` instance — no shared state.
