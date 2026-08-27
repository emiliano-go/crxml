"""rypipe adapter integration for crxml.

When rypipe is installed, this module registers a `crxml` adapter so users
can write::

    import rypipe
    table = rypipe.read("report.xml", format="crxml", row_tag="Row")

The adapter delegates parsing to the existing `CrystalXMLSource`, so all
engine selection, memory budgets, and pushdown filters continue to work.
"""

from __future__ import annotations

from typing import Any

from .source import CrystalXMLSource


class CrystalXMLAdapter:
    """rypipe-compatible adapter for Crystal Reports XML files."""

    def read(self, path: str, **kwargs: Any) -> Any:
        """Parse ``path`` and return a ``pyarrow.Table``."""
        return CrystalXMLSource(path, **kwargs).to_arrow()

    def iter_record_batches(
        self, path: str, memory: str | int = "64MiB", batch_size: int | None = None, **kwargs: Any
    ):
        """Yield ``pyarrow.RecordBatch`` objects with constant memory.

        Peak is ``memory`` + one batch. Use ``memory="64KB"`` and
        ``batch_size=1`` for minimal footprint (Rust-only 64 KB).
        """
        yield from CrystalXMLSource(path, **kwargs).iter_record_batches(
            memory=memory, batch_size=batch_size
        )


def _register() -> None:
    """Register the adapter with rypipe if it is available."""
    try:
        import rypipe
    except Exception:  # pragma: no cover (rypipe is optional)
        return

    rypipe.register_adapter("crxml", CrystalXMLAdapter(), extensions=[".xml"])


_register()
