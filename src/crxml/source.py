import logging
import os
import re
import warnings
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

    Compatibility helper: for columnar/parallel engines when row iteration
    is requested.  Table-oriented callers should use ``to_arrow()`` or
    ``to_dataframe()`` directly to avoid the dict reconstruction overhead.
    """
    for batch in table.to_batches():
        yield from batch.to_pylist()


def _batch_iter(reader, batch_size: int = 1024) -> Iterator[dict]:
    """Batched row iterator backed by CrxmlReader.next_batch.

    One Rust call per batch; ``yield from`` walks each batch list at
    C speed (no per-row Python-level __next__ or index bookkeeping).
    """
    while True:
        batch = reader.next_batch(batch_size)
        if batch is None:
            return
        yield from batch


def _validate_filter(f: dict) -> None:
    """Eagerly validate a pushdown filter spec (mirrors the Rust planner).

    Raises ``ValueError`` at construction time instead of deep inside a
    later ``to_arrow()`` call.
    """
    if not isinstance(f, dict):
        raise ValueError(
            f"filter must be a dict, got {type(f).__name__}: {f!r}"
        )
    op = f.get("op")
    if op is None:
        raise ValueError("filter must include an 'op' key")
    compare_ops = {">", "<", ">=", "<=", "==", "!=", "gt", "lt", "ge", "le", "eq", "ne"}
    constant_ops = {"==", "!=", "eq", "ne"}
    if "field_a" in f or "field_b" in f:
        if not ("field_a" in f and "field_b" in f):
            raise ValueError(
                "column-to-column filter requires both 'field_a' and 'field_b'"
            )
        if op not in compare_ops:
            raise ValueError(
                f"unsupported column-compare op {op!r}; valid ops: "
                f"> < >= <= == != (or gt lt ge le eq ne)"
            )
        return
    if "field" not in f or "value" not in f:
        raise ValueError(
            "filter requires either 'field' + 'op' + 'value', or "
            "'field_a' + 'op' + 'field_b'"
        )
    if op not in constant_ops:
        raise ValueError(
            f"unsupported constant-filter op {op!r}; valid ops: "
            f"'=='/'eq', '!='/'ne'"
        )


class CrystalXMLSource:
    """Streaming/columnar source over one Crystal Reports XML file.

    Table sinks (``to_arrow``/``to_pandas``/...) cache the parsed Arrow
    table on first call, so repeated sinks on the same source parse only
    once.  The cache is *not* thread-safe and holds the full table in
    memory; call :meth:`clear_cache` to release it, or create separate
    source objects per thread.  Row iteration (``iter(source)``) always
    re-reads the file and never populates the cache.
    """

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
        "_schema_discovered",
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
        dictionary_columns: Optional[list[str]] = None,
        schema: Optional[list[str]] = None,
        auto_dict: bool = False,
        use_mmap: bool = True,
        batch_size: int = 1024,
    ):
        self._filepath = Path(source)
        if not self._filepath.exists():
            raise FileNotFoundError(f"File not found: {self._filepath}")

        self._row_tag = row_tag
        self._memory = _parse_memory(memory)
        # Auto-tune: separate optima (Aug 28 sweep, now with frozen schema).
        # Full-RAM par peaks at 4 MB (par133 4450 vs par266 4328 at 2 MB, −3%;
        # 1 MB 3553 collapses). Streaming auto peaks at 2 MB but is −14% vs par
        # due to Discovery; explicit schema 4980 beats par. 8×threads capped
        # 533 MB at 128 vs ideal 133 for 4 MB. Raise to 16×threads to clear it
        # while still bounding 50 GB (16×16=256 chunks, 195 MB/chunk at 50 GB).
        t = threads if threads > 0 else _default_threads()
        file_bytes = self._filepath.stat().st_size
        self._num_chunks = max(t, min(16 * t, file_bytes // (4 * 1024 * 1024)))
        self._field_mapping = field_mapping or {}
        self._drop_fields = drop_fields or []
        self._filter = filter
        if self._filter is not None:
            _validate_filter(self._filter)
        self._field_types = field_types or {}
        self._dictionary_columns = dictionary_columns or []
        self._use_mmap = use_mmap
        self._schema = schema or []
        self._schema_discovered = bool(self._schema)
        self._auto_dict = auto_dict
        self._batch_size = batch_size
        self._cached_arrow = None

        if engine not in ("auto", "stream", "columnar", "parallel"):
            raise ValueError(
                f"engine must be 'auto', 'stream', 'columnar', or 'parallel', "
                f"got {engine!r}"
            )

        self._engine_desired = engine

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

    def _build_bounded_kwargs(self) -> dict:
        return {
            "field_mapping": self._field_mapping or None,
            "drop_fields": self._drop_fields or None,
            "filter": self._filter or None,
            "field_types": self._field_types or None,
            "dictionary_columns": self._dictionary_columns or None,
            "schema": self._schema or None,
            "auto_dict": self._auto_dict,
            "prefault": False,
        }

    def _read_arrow(self, plan_overrides=None):
        if self._cached_arrow is not None and plan_overrides is None:
            return self._cached_arrow
        engine = self._resolve_engine("table")
        plan = self._build_plan_kwargs()
        if plan_overrides:
            plan.update(plan_overrides)
        if (
            self._memory is not None
            and self._filepath.stat().st_size > self._memory
            and _HAS_BOUNDED
        ):
            bounded_kwargs = self._build_bounded_kwargs()
            if plan_overrides:
                bounded_kwargs.update(plan_overrides)
            table = _core.read_to_columnar_bounded(
                str(self._filepath), self._row_tag, self._memory,
                **bounded_kwargs,
            )
        elif engine == "columnar":
            table = _core.read_to_columnar(
                str(self._filepath), self._row_tag,
                prefault=self._use_mmap, **plan
            )
        elif engine == "parallel":
            table = _core.read_to_columnar_par(
                str(self._filepath), self._row_tag, self._num_chunks,
                prefault=self._use_mmap, **plan
            )
        else:
            import pyarrow as pa
            rows = []
            for batch in self._iter_batches():
                rows.extend(batch)
            if not rows:
                table = pa.table({})
            else:
                # Handle sparse rows (e.g., FieldG present in 30% of rows)
                all_keys = set()
                for r in rows:
                    all_keys.update(r.keys())
                table = pa.table({k: [r.get(k) for r in rows] for k in all_keys})
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

        for batch in self.to_arrow().to_batches(max_chunksize=batch_size):
            yield batch.to_pylist()

    def __iter__(self) -> Iterator[dict]:
        engine = self._resolve_engine("iter")

        if engine == "stream":
            return _batch_iter(self._stream_iter(), batch_size=self._batch_size)

        return _arrow_iter(self._read_arrow())

    def to_dataframe(self, dtype_backend: str = "pyarrow") -> "pd.DataFrame":
        return self.to_pandas(dtype_backend=dtype_backend)

    def to_arrow(self, combine: bool = False):
        """Return a ``pyarrow.Table``, optionally with chunked columns.

        By default ``combine=False`` keeps chunked columns from parallel parsing
        (no serial ``combine_chunks`` copy, ~11% on 1 GB `par16` 380→338 ms).
        Pass ``combine=True`` if you need a single contiguous `ChunkedArray`
        for ``zero_copy_only`` or `chunk(0)` patterns.
        """
        tbl = self._read_arrow()
        if combine and tbl is not None:
            tbl = tbl.combine_chunks()
        return tbl

    def clear_cache(self):
        """Drop the cached Arrow table (see class docstring)."""
        self._cached_arrow = None

    def to_polars(self):
        import polars as pl

        return pl.from_arrow(self.to_arrow())

    def to_pandas(self, dtype_backend: str = "pyarrow") -> "pd.DataFrame":
        import pandas as pd

        table = self.to_arrow()
        if dtype_backend == "pyarrow":
            return table.to_pandas(types_mapper=pd.ArrowDtype)
        return table.to_pandas()

    def to_parquet(self, path: Union[str, Path], **kwargs):
        import pyarrow.parquet as pq

        pq.write_table(self.to_arrow(), str(path), **kwargs)

    def iter_record_batches(
        self, memory: Union[str, int] = "64MiB", batch_size: Optional[int] = None,
        threads: Optional[int] = None,
    ) -> Iterator["pa.RecordBatch"]:
        """Yield Arrow ``RecordBatch`` objects with constant memory.

        Unlike ``to_arrow()`` (which materializes a full table) or
        ``iter_batches`` (which materializes then splits), this streams
        directly from Rust via ``BatchConsumer`` and ``StreamingBatchIterator``.
        Peak is ``memory`` + one batch + export buffer — set ``memory="64KB"``
        and ``batch_size=1`` for the smallest footprint (one row per batch,
        ~1 KB for CR rows). Python overhead means true 64 KB is only reachable
        from Rust, but this is still bounded for 50 GB files.

        Examples
        --------
        >>> import pyarrow.parquet as pq
        >>> src = CrystalXMLSource("50GB.xml", row_tag="Details")
        >>> writer = pq.ParquetWriter("out.parquet", src.to_arrow().schema)
        >>> for batch in src.iter_record_batches(memory="64KB"):
        ...     writer.write_batch(batch)
        >>> writer.close()
        """
        # Use the Rust streaming iterator directly — no Vec<RecordBatch> collection.
        # _core.iter_record_batches is the true 64KB path (mmap + reusable buffer).
        if batch_size is not None:
            warnings.warn(
                "batch_size is ignored; batch size is derived from the memory budget. "
                "Pass memory='1MB' (default) for ~895 rows/batch.",
                DeprecationWarning,
                stacklevel=2,
            )
        # Auto-discover schema on first call so repeat parses skip discovery.
        # This makes auto match explicit-schema performance after the first call.
        if not self._schema_discovered:
            self._schema = _core.discover_schema(
                str(self._filepath),
                row_tag=self._row_tag,
                field_mapping=self._field_mapping or None,
                drop_fields=self._drop_fields or None,
                filter=self._filter or None,
                field_types=self._field_types or None,
                dictionary_columns=self._dictionary_columns or None,
                auto_dict=self._auto_dict,
            )
            self._schema_discovered = True
        yield from _core.iter_record_batches(
            str(self._filepath),
            row_tag=self._row_tag,
            memory=str(memory) if isinstance(memory, int) else memory,
            batch_size=batch_size,
            threads=threads,
            **self._build_plan_kwargs(),
        )

    def __or__(self, stage):
        from .pipeline import Pipeline

        return Pipeline(self) | stage


def discover_schema(
    source: Union[str, Path],
    *,
    row_tag: str = "Details",
    field_mapping: Optional[dict[str, str]] = None,
    drop_fields: Optional[list[str]] = None,
    filter: Optional[dict[str, str]] = None,
    field_types: Optional[dict[str, str]] = None,
    dictionary_columns: Optional[list[str]] = None,
    schema: Optional[list[str]] = None,
    auto_dict: bool = False,
) -> list[str]:
    """Discover the frozen schema for a file (reusable across batch workloads).

    Scans `source` once (full scan for ≤128 MB, else 16×2 MiB sampled windows
    in parallel via `rayon`) and returns the column names in file order after
    applying `field_mapping`/`drop_fields`/`filter` etc. Pass the result as
    ``CrystalXMLSource(..., schema=schema).iter_record_batches(...)`` to avoid
    per-file Discovery (≈5 ms on 533 MB, ~19 ms serial before parallelisation)
    and hit the explicit fast path (4980 MB/s vs 3828 auto on 533 MB).

    Example
    -------
    >>> schema = crxml.discover_schema("sample.xml")
    >>> for f in files:
    ...     for batch in CrystalXMLSource(f, schema=schema).iter_record_batches(memory="64MB", threads=16):
    ...         writer.write_batch(batch)
    """
    return _core.discover_schema(
        str(source),
        row_tag=row_tag,
        field_mapping=field_mapping,
        drop_fields=drop_fields,
        filter=filter,
        field_types=field_types,
        dictionary_columns=dictionary_columns,
        schema=schema,
        auto_dict=auto_dict,
    )
