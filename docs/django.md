# Django Integration

Use crxml inside a Django management command to import Crystal Reports XML
data into your models.

## Management command

```python
# yourapp/management/commands/import_report.py
from django.core.management.base import BaseCommand
from crxml import CrystalXMLSource, RenameFields, CastTypes, collect

from yourapp.models import Invoice


class Command(BaseCommand):
    help = "Import a Crystal Reports XML file into Invoice models"

    def add_arguments(self, parser):
        parser.add_argument("path", type=str)
        parser.add_argument("--batch", type=int, default=1000)

    def handle(self, *args, **options):
        pipeline = (
            CrystalXMLSource(options["path"])
            | RenameFields({
                "{Report.InvoiceNo}": "number",
                "{Report.Customer}": "customer",
                "{Report.Amount}": "amount",
                "{Report.Date}": "date",
            })
            | CastTypes({"amount": float, "date": str})
        )

        batch = []
        for row in pipeline:
            batch.append(Invoice(
                number=row["number"],
                customer=row["customer"],
                amount=row["amount"],
                date=row["date"],
            ))
            if len(batch) >= options["batch"]:
                Invoice.objects.bulk_create(batch)
                self.stdout.write(f"Imported {len(batch)} invoices")
                batch.clear()

        if batch:
            Invoice.objects.bulk_create(batch)
            self.stdout.write(f"Imported {len(batch)} invoices")
```

Run it:

```bash
python manage.py import_report report.xml --batch 2000
```

## Upload + preview

A simple admin-like view that accepts a file upload, parses it, and renders
a preview table:

```python
# yourapp/views.py
import os
import tempfile

from django.shortcuts import render
from django.http import HttpResponseRedirect
from django.urls import reverse
from crxml import CrystalXMLSource

from .forms import ReportUploadForm


def preview_report(request):
    if request.method == "POST":
        form = ReportUploadForm(request.POST, request.FILES)
        if form.is_valid():
            with tempfile.NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
                tmp.write(request.FILES["file"].read())
                tmp_path = tmp.name
            try:
                source = CrystalXMLSource(tmp_path, row_tag="Details")
                rows = [row for row in source]
                return render(request, "preview.html", {
                    "fields": list(rows[0].keys()) if rows else [],
                    "rows": rows[:100],
                    "total": len(rows),
                })
            finally:
                os.unlink(tmp_path)
    else:
        form = ReportUploadForm()

    return render(request, "upload.html", {"form": form})
```

## Periodic import with Celery

```python
# yourapp/tasks.py
from celery import shared_task
from crxml import CrystalXMLSource, collect

from .models import SalesRecord


@shared_task
def import_sales_report(path: str):
    src = CrystalXMLSource(path, row_tag="Details")
    records = []
    for row in src:
        records.append(SalesRecord(
            product=row.get("{Report.Product}", ""),
            quantity=int(row.get("{Report.Qty}", 0)),
            price=float(row.get("{Report.Price}", 0)),
        ))

    SalesRecord.objects.bulk_create(records, ignore_conflicts=True)
    return len(records)
```

## Admin action for file upload

Add a Django admin action that accepts a file, parses it, and imports into
a model:

```python
# yourapp/admin.py
import os
import tempfile

from django.contrib import admin, messages
from django import forms
from django.shortcuts import render
from crxml import CrystalXMLSource, collect

from .models import Invoice


class UploadXMLForm(forms.Form):
    file = forms.FileField()


@admin.action(description="Import from Crystal Reports XML")
def import_from_xml(modeladmin, request, queryset):
    if "file" not in request.FILES:
        if request.method == "POST":
            form = UploadXMLForm(request.POST, request.FILES)
            if form.is_valid():
                with tempfile.NamedTemporaryFile(delete=False, suffix=".xml") as tmp:
                    tmp.write(request.FILES["file"].read())
                    tmp_path = tmp.name
                try:
                    source = CrystalXMLSource(tmp_path, row_tag="Details")
                    rows = collect(source)
                    for row in rows:
                        Invoice.objects.create(
                            number=row.get("{Report.InvoiceNo}", ""),
                            customer=row.get("{Report.Customer}", ""),
                            amount=row.get("{Report.Amount}", 0),
                        )
                    modeladmin.message_user(request, f"Imported {len(rows)} invoices")
                finally:
                    os.unlink(tmp_path)
                return
        else:
            form = UploadXMLForm()
        return render(request, "admin/upload_xml.html", {"form": form})
    return


@admin.register(Invoice)
class InvoiceAdmin(admin.ModelAdmin):
    actions = [import_from_xml]
```

## Celery task with progress tracking

Track import progress using the Celery task state:

```python
# yourapp/tasks.py
from celery import shared_task, current_task
from crxml import CrystalXMLSource, RenameFields, CastTypes
from django.db import transaction

from .models import SalesRecord


@shared_task(bind=True)
def import_report(self, path: str):
    pipe = (
        CrystalXMLSource(path, row_tag="Details")
        | RenameFields({
            "{Report.Product}": "product",
            "{Report.Qty}": "quantity",
            "{Report.Price}": "price",
        })
        | CastTypes({"quantity": int, "price": float})
    )

    batch = []
    total = 0
    for row in pipe:
        batch.append(SalesRecord(
            product=row["product"],
            quantity=row["quantity"],
            price=row["price"],
        ))
        if len(batch) >= 1000:
            with transaction.atomic():
                SalesRecord.objects.bulk_create(batch, ignore_conflicts=True)
            total += len(batch)
            current_task.update_state(
                state="PROGRESS",
                meta={"current": total}
            )
            batch.clear()

    if batch:
        with transaction.atomic():
            SalesRecord.objects.bulk_create(batch, ignore_conflicts=True)
        total += len(batch)

    return {"imported": total}
```

Query the result from the view:

```python
from celery.result import AsyncResult


def task_status(request, task_id):
    result = AsyncResult(task_id)
    return JsonResponse({
        "state": result.state,
        "progress": result.info.get("current", 0) if result.info else 0,
    })
```

## Thread safety

Django's ORM is thread-safe. Each request or task gets its own
`CrystalXMLSource` instance, so there is no shared state across requests.
