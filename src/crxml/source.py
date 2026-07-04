import logging
import os
import re
from pathlib import Path
from typing import Iterator, Optional, Union

from crxml import _crxml_core as _core

CrxmlReader = _core.CrxmlReader

_HAS_COLUMNAR = hasattr(_core, "read_to_columnar")
_HAS_PARALLEL = hasattr(_core, "read_to_columnar_par")
_HAS_BOUNDED = hasattr(_core, "read_to_columnar_bounded")

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
    """Yield dicts from a pyarrow Table.

    Compatibility helper — for columnar/parallel engines when row iteration
    is requested.  Table-oriented callers should use ``to_arrow()`` or
    ``to_dataframe()`` directly to avoid the dict reconstruction overhead.
    """
    for i in range(table.num_rows):
        yield {col: table.column(col)[i].as_py() for col in table.column_names}


class _BatchIter:
    """Batched row iterator backed by CrxmlReader.next_batch.

    Reduces Python↔Rust boundary crossings from one-per-row to one-per-batch.
    """

    __slots__ = ("_reader", "_n", "_batch", "_i")

    def __init__(self, reader, batch_size: int = 1024):
        self._reader = reader
        self._n = batch_size
        self._batch: list[dict] = []
        self._i = 0

    def __iter__(self):
        return self

    def __next__(self) -> dict:
        if self._i >= len(self._batch):
            b = self._reader.next_batch(self._n)
            if b is None:
                raise StopIteration
            self._batch = b
            self._i = 0
        item = self._batch[self._i]
        self._i += 1
        return item


class CrystalXMLSource:
    __slots__ = (
        "_row_tag",
        "_filepath",
        "_engine",
        "_engine_desired",
        "_num_chunks",
        "_memory",
        "_field_mapping",
        "_drop_fields",
        "_filter",
        "_field_types",
        "_dictionary_columns",
        "_use_mmap",
        "_schema",
        "_auto_dict",
        "_batch_size",
        "_cached_arrow",
    )

    def __init__(
        self,
        source: Union[str, Path],
        *,
        row_tag: str = "Row",
        engine: str = "auto",
        threads: int = 0,
        memory: Optional[Union[str, int]] = None,
        field_mapping: Optional[dict[str, str]] = None,
        drop_fields: Optional[list[str]] = None,
        filter: Optional[dict[str, str]] = None,
        field_types: Optional[dict[str, str]] = None,
        #   Example: field_types={"Score": "int64", "Amount": "float64", "IsValid": "bool"}
        #   Typed columns set unparseable values to null (cross-chunk conflict = null).
        dictionary_columns: Optional[list[str]] = None,
        schema: Optional[list[str]] = None,
        auto_dict: bool = False,
        use_mmap: bool = False,
        batch_size: int = 1024,
    ):
        self._filepath = Path(source)
        if not self._filepath.exists():
            raise FileNotFoundError(f"File not found: {self._filepath}")

        self._row_tag = row_tag
        self._memory = _parse_memory(memory)
        self._num_chunks = threads if threads > 0 else _default_threads()
        self._field_mapping = field_mapping or {}
        self._drop_fields = drop_fields or []
        self._filter = filter  # {"field": ..., "op": ..., "value": ...} or None
        self._field_types = field_types or {}
        self._dictionary_columns = dictionary_columns or []
        self._use_mmap = use_mmap
        self._schema = schema or []
        self._auto_dict = auto_dict
        self._batch_size = batch_size
        self._cached_arrow = None

        if engine not in ("auto", "stream", "columnar", "parallel"):
            raise ValueError(
                f"engine must be 'auto', 'stream', 'columnar', or 'parallel', "
                f"got {engine!r}"
            )

        self._engine_desired = engine

        # Construction-time resolution (validates feature availability).
        if engine == "auto":
            self._engine = "stream"
        else:
            self._engine = engine

        if self._engine in ("columnar", "parallel") and not _HAS_COLUMNAR:
            raise RuntimeError(
                "Columnar/parallel engine requires the 'columnar' Cargo feature. "
                "Rebuild with: PYO3_USE_ABI3_FORWARD_COMPATIBILITY=1 "
                "pip install -e . --config-settings=--features=columnar"
            )

    def _resolve_engine(self, goal: str) -> str:
        """Return engine for *goal* ('iter' or 'table').

        When ``engine='auto'``, iteration always uses the stream (dict) path
        and table methods (to_arrow/to_dataframe) use the columnar/parallel
        path — avoiding the dict loop entirely for the DataFrame use case.
        """
        explicit = self._engine_desired

        if explicit != "auto":
            return self._engine

        size = self._filepath.stat().st_size
        mem_ok = self._memory is None or size <= self._memory

        if goal == "iter":
            logger.info(
                "engine=auto → iter → stream (%.1f MB file)",
                size / 1e6,
            )
            return "stream"

        # goal == "table": route to columnar (parallel if large enough).
        if size >= 8 * 1024 * 1024 and _HAS_PARALLEL and mem_ok:
            logger.info(
                "engine=auto → table → parallel (%.1f MB file, %d threads)",
                size / 1e6,
                self._num_chunks,
            )
            return "parallel"

        if _HAS_COLUMNAR and mem_ok:
            logger.info(
                "engine=auto → table → columnar (%.1f MB file)",
                size / 1e6,
            )
            return "columnar"

        logger.info(
            "engine=auto → table → stream (fallback, %.1f MB file)",
            size / 1e6,
        )
        return "stream"

    def _build_plan_kwargs(self) -> dict:
        kwargs = {"use_mmap": self._use_mmap}
        if self._field_mapping:
            kwargs["field_mapping"] = self._field_mapping
        if self._drop_fields:
            kwargs["drop_fields"] = self._drop_fields
        if self._filter:
            kwargs["filter"] = self._filter
        if self._field_types:
            kwargs["field_types"] = self._field_types
        if self._dictionary_columns:
            kwargs["dictionary_columns"] = self._dictionary_columns
        if self._schema:
            kwargs["schema"] = self._schema
        kwargs["auto_dict"] = self._auto_dict
        return kwargs

    def _read_arrow(self, plan_overrides=None):
        """Parse to a pyarrow.Table.

        Parameters
        ----------
        plan_overrides : dict or None
            Extra BuildPlan kwargs merged on top of the source's own
            configuration.  Used by pipeline stage fusion (Phase 4) to
            avoid a dict round-trip for fusible stages.
            When supplied the cache is bypassed.
        """
        if self._cached_arrow is not None and plan_overrides is None:
            return self._cached_arrow
        engine = self._resolve_engine("table")
        plan = self._build_plan_kwargs()
        if plan_overrides:
            plan.update(plan_overrides)
        # Use bounded-batch parsing when memory budget is specified and
        # the file is larger than the budget (for columnar/parallel engines)
        if (
            self._memory is not None
            and self._filepath.stat().st_size > self._memory
            and _HAS_BOUNDED
            and engine in ("columnar", "parallel")
        ):
            kwargs_flat = {
                k: v for k, v in plan.items()
                if k not in ("use_mmap", "num_chunks")
            }
            table = _core.read_to_columnar_bounded(
                str(self._filepath), self._row_tag, self._memory, **kwargs_flat
            )
        elif engine == "columnar":
            table = _core.read_to_columnar(
                str(self._filepath), self._row_tag, **plan
            )
        elif engine == "parallel":
            table = _core.read_to_columnar_par(
                str(self._filepath), self._row_tag, self._num_chunks, **plan
            )
        else:
            import pyarrow as pa
            rows = []
            for batch in self._iter_batches():
                rows.extend(batch)
            if not rows:
                table = pa.table({})
            else:
                table = pa.table({k: [r[k] for r in rows] for k in rows[0]})
        if plan_overrides is None:
            self._cached_arrow = table
        return table

    def schema(self) -> list[str]:
        first_row = next(iter(self), None)
        if first_row is None:
            return []
        return [*first_row]

    def _stream_iter(self):
        return CrxmlReader(str(self._filepath), self._row_tag)

    def _iter_batches(self, batch_size: int | None = None):
        if batch_size is None:
            batch_size = self._batch_size

        engine = self._resolve_engine("iter")

        if engine == "stream":
            reader = self._stream_iter()
            while True:
                batch = reader.next_batch(batch_size)
                if batch is None:
                    break
                yield batch
            return

        rows = self.to_arrow().to_pylist()
        for i in range(0, len(rows), batch_size):
            yield rows[i : i + batch_size]

    def __iter__(self) -> Iterator[dict]:
        engine = self._resolve_engine("iter")

        if engine == "stream":
            return _BatchIter(self._stream_iter(), batch_size=self._batch_size)

        return _arrow_iter(self._read_arrow())

    def to_dataframe(self) -> "pd.DataFrame":
        return self.to_pandas()

    def to_arrow(self):
        """Return a pyarrow.Table of the parsed data."""
        return self._read_arrow()

    def to_polars(self):
        """Return a polars DataFrame (zero-copy from Arrow)."""
        import polars as pl

        return pl.from_arrow(self.to_arrow())

    def to_pandas(self, arrow_backed: bool = True) -> "pd.DataFrame":
        """Return a pandas DataFrame.

        Parameters
        ----------
        arrow_backed : bool
            If True (default), use ``pd.ArrowDtype`` for zero-copy string
            columns (requires pandas ≥ 1.5).  If False, materialise
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
