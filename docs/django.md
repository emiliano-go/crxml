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
from django.shortcuts import render
from django.http import HttpResponseRedirect
from django.urls import reverse
from crxml import CrystalXMLSource

from .forms import ReportUploadForm


def preview_report(request):
    if request.method == "POST":
        form = ReportUploadForm(request.POST, request.FILES)
        if form.is_valid():
            src = CrystalXMLSource(request.FILES["file"], row_tag="Details")
            rows = [row for row in src]
            return render(request, "preview.html", {
                "fields": list(rows[0].keys()) if rows else [],
                "rows": rows[:100],
                "total": len(rows),
            })
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

## Thread safety

Django's ORM is thread-safe. Each request or task gets its own
`CrystalXMLSource` instance, so there is no shared state across requests.
