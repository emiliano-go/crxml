import os
from pathlib import Path
from typing import Iterator, Union

from crxml._crxml_core import CrxmlReader

_HAS_COLUMNAR = hasattr(CrxmlReader, "read_to_columnar")
_HAS_PARALLEL = hasattr(CrxmlReader, "read_to_columnar_par")


class CrystalXMLSource:
    __slots__ = ("_row_tag", "_filepath", "_engine", "_num_chunks")

    def __init__(
        self,
        source: Union[str, Path],
        *,
        row_tag: str = "Row",
        engine: str = "stream",
        num_chunks: int = 0,
    ):
        if engine not in ("stream", "columnar", "parallel"):
            raise ValueError(
                f"engine must be 'stream', 'columnar', or 'parallel', got {engine!r}"
            )
        if engine == "columnar" and not _HAS_COLUMNAR:
            raise RuntimeError(
                "Columnar engine requires the 'columnar' Cargo feature. "
                "Rebuild with: PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 pip install -e . --config-settings=--features=columnar"
            )
        if engine == "parallel" and not _HAS_PARALLEL:
            raise RuntimeError(
                "Parallel engine requires the 'columnar' Cargo feature. "
                "Rebuild with: PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 pip install -e . --config-settings=--features=columnar"
            )
        self._engine = engine
        self._row_tag = row_tag
        self._filepath = Path(source)
        self._num_chunks = num_chunks or os.cpu_count() or 4

        if not self._filepath.exists():
            raise FileNotFoundError(f"File not found: {self._filepath}")

    def schema(self) -> list[str]:
        first_row = next(iter(self), None)
        if first_row is None:
            return []
        return [*first_row]

    def __iter__(self) -> Iterator[dict]:
        if self._engine == "stream":
            return CrxmlReader(str(self._filepath), self._row_tag)
        if self._engine == "parallel":
            return _parallel_iter(self._filepath, self._row_tag, self._num_chunks)
        return _columnar_iter(self._filepath, self._row_tag)

    def to_dataframe(self) -> "pd.DataFrame":
        if self._engine == "columnar":
            import pandas as pd
            table = CrxmlReader.read_to_columnar(str(self._filepath), self._row_tag)
            return table.to_pandas()
        if self._engine == "parallel":
            import pandas as pd
            table = CrxmlReader.read_to_columnar_par(
                str(self._filepath), self._row_tag, self._num_chunks
            )
            return table.to_pandas()
        from .sinks import to_dataframe
        return to_dataframe(self)

    def __or__(self, stage):
        from .pipeline import Pipeline
        return Pipeline(self) | stage


def _columnar_iter(filepath: Path, row_tag: str) -> Iterator[dict]:
    table = CrxmlReader.read_to_columnar(str(filepath), row_tag)
    for i in range(table.num_rows):
        yield {col: table.column(col)[i].as_py() for col in table.column_names}


def _parallel_iter(filepath: Path, row_tag: str, num_chunks: int) -> Iterator[dict]:
    table = CrxmlReader.read_to_columnar_par(str(filepath), row_tag, num_chunks)
    for i in range(table.num_rows):
        yield {col: table.column(col)[i].as_py() for col in table.column_names}
