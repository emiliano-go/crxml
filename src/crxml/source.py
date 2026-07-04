import logging
import os
import re
from pathlib import Path
from typing import Iterator, Optional, Union

from crxml._crxml_core import CrxmlReader

_HAS_COLUMNAR = hasattr(CrxmlReader, "read_to_columnar")
_HAS_PARALLEL = hasattr(CrxmlReader, "read_to_columnar_par")

logger = logging.getLogger("crxml")


def _parse_memory(value: Optional[Union[str, int]]) -> Optional[int]:
    if value is None:
        return None
    if isinstance(value, int):
        return value
    m = re.match(r"^(\d+(?:\.\d+)?)\s*(KB|MB|GB|TB)?$", value.strip(), re.IGNORECASE)
    if not m:
        raise ValueError(
            f"memory must be None, an int (bytes), or a string like '8GB', got {value!r}"
        )
    num = float(m.group(1))
    unit = (m.group(2) or "GB").upper()
    multipliers = {"KB": 1024, "MB": 1024**2, "GB": 1024**3, "TB": 1024**4}
    return int(num * multipliers[unit])


def _default_threads() -> int:
    return os.cpu_count() or 4


def _arrow_iter(table) -> Iterator[dict]:
    for i in range(table.num_rows):
        yield {col: table.column(col)[i].as_py() for col in table.column_names}


class CrystalXMLSource:
    __slots__ = (
        "_row_tag",
        "_filepath",
        "_engine",
        "_num_chunks",
        "_memory",
    )

    def __init__(
        self,
        source: Union[str, Path],
        *,
        row_tag: str = "Row",
        engine: str = "auto",
        threads: int = 0,
        memory: Optional[Union[str, int]] = None,
    ):
        self._filepath = Path(source)
        if not self._filepath.exists():
            raise FileNotFoundError(f"File not found: {self._filepath}")

        self._row_tag = row_tag
        self._memory = _parse_memory(memory)
        self._num_chunks = threads if threads > 0 else _default_threads()

        if engine not in ("auto", "stream", "columnar", "parallel"):
            raise ValueError(
                f"engine must be 'auto', 'stream', 'columnar', or 'parallel', "
                f"got {engine!r}"
            )

        if engine == "auto":
            size = self._filepath.stat().st_size
            mem_ok = self._memory is None or size <= self._memory
            if size >= 8 * 1024 * 1024 and _HAS_PARALLEL and mem_ok:
                self._engine = "parallel"
                logger.info(
                    "engine=auto → parallel (%.1f MB file, %d threads)",
                    size / 1e6,
                    self._num_chunks,
                )
            elif _HAS_COLUMNAR and mem_ok:
                self._engine = "columnar"
                logger.info("engine=auto → columnar (%.1f MB file)", size / 1e6)
            else:
                self._engine = "stream"
                logger.info("engine=auto → stream (%.1f MB file)", size / 1e6)
        else:
            self._engine = engine

        if self._engine in ("columnar", "parallel") and not _HAS_COLUMNAR:
            raise RuntimeError(
                "Columnar/parallel engine requires the 'columnar' Cargo feature. "
                "Rebuild with: PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 "
                "pip install -e . --config-settings=--features=columnar"
            )

    def _read_arrow(self):
        if self._engine == "columnar":
            return CrxmlReader.read_to_columnar(str(self._filepath), self._row_tag)
        if self._engine == "parallel":
            return CrxmlReader.read_to_columnar_par(
                str(self._filepath), self._row_tag, self._num_chunks
            )
        import pyarrow as pa

        rows = list(self._stream_iter())
        if not rows:
            return pa.table({})
        return pa.table({k: [r[k] for r in rows] for k in rows[0]})

    def _stream_iter(self):
        return CrxmlReader(str(self._filepath), self._row_tag)

    def schema(self) -> list[str]:
        first_row = next(iter(self), None)
        if first_row is None:
            return []
        return [*first_row]

    def _stream_iter(self):
        return CrxmlReader(str(self._filepath), self._row_tag)

    def __iter__(self) -> Iterator[dict]:
        if self._engine == "stream" or (
            self._engine in ("columnar", "parallel")
            and not _HAS_COLUMNAR
        ):
            return self._stream_iter()

        return _arrow_iter(self._read_arrow())

    def to_dataframe(self) -> "pd.DataFrame":
        if self._engine == "stream":
            from .sinks import to_dataframe

            return to_dataframe(self)
        return self.to_pandas()

    def to_arrow(self):
        """Return a pyarrow.Table of the parsed data."""
        return self._read_arrow()

    def to_polars(self):
        """Return a polars DataFrame (zero-copy from Arrow)."""
        import polars as pl

        return pl.from_arrow(self.to_arrow())

    def to_pandas(self, arrow_backed: bool = False) -> "pd.DataFrame":
        """Return a pandas DataFrame.

        Parameters
        ----------
        arrow_backed : bool
            If True, use ``pd.ArrowDtype`` for zero-copy string columns
            (requires pandas ≥ 1.5).  If False (default), materialise
            strings as Python ``str`` objects.
        """
        import pandas as pd

        table = self.to_arrow()
        if arrow_backed:
            return table.to_pandas(types_mapper=pd.ArrowDtype)
        return table.to_pandas()

    def to_parquet(self, path: Union[str, Path], **kwargs):
        """Write the data to a Parquet file.

        Parameters
        ----------
        path : str or Path
            Destination file path.
        **kwargs
            Forwarded to ``pyarrow.parquet.write_table``.
        """
        import pyarrow.parquet as pq

        pq.write_table(self.to_arrow(), str(path), **kwargs)

    def __or__(self, stage):
        from .pipeline import Pipeline

        return Pipeline(self) | stage
