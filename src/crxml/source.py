from pathlib import Path
from typing import Iterator, Union

from crxml._crxml_core import CrxmlReader


class CrystalXMLSource:
    def __init__(self, source: Union[str, Path], *, row_tag: str = "Row"):
        self._row_tag = row_tag
        self._filepath = Path(source)

        if not self._filepath.exists():
            raise FileNotFoundError(f"File not found: {self._filepath}")

    def schema(self) -> list[str]:
        """Return the field names of the first row without consuming the main stream."""
        first_row = next(iter(self), None)
        if first_row is None:
            return []
        return list(first_row.keys())

    def __iter__(self) -> Iterator[dict]:
        return CrxmlReader(str(self._filepath), self._row_tag)

    def __or__(self, stage):
        from .pipeline import Pipeline
        return Pipeline(self) | stage
