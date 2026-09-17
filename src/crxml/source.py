import logging
import os
import re
import warnings
from pathlib import Path
from typing import Iterator, Optional, Union

from rypipe import Adapter
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
    m = re.match(r"^(\d+(?:\.\d+)?)\s*(KiB|MiB|GiB|TiB|KB|MB|GB|TB)?$", value.strip())
    if not m:
        raise ValueError(
            f"memory must be None, an int (bytes), or a string like '8GB', got {value!r}"
        )
    num = float(m.group(1))
    unit = (m.group(2) or "GB")
    multipliers = {
        "KB": 1024, "KiB": 1024,
        "MB": 1024**2, "MiB": 1024**2,
        "GB": 1024**3, "GiB": 1024**3,
        "TB": 1024**4, "TiB": 1024**4,
    }
    return int(num * multipliers[unit])


def _default_threads() -> int:
    return os.cpu_count() or 4


def _arrow_iter(table) -> Iterator[dict]:
    """Yield dicts from a pyarrow Table.

    Compatibility helper: for columnar/parallel engines when row iteration
    is requested.  Table-oriented callers should use ``to_arrow()`` or
    ``to_pandas()`` directly to avoid the dict reconstruction overhead.
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
    later ``to_arrow()`` call. Accepts compound specs
    (``{"and": [...]}`` / ``{"or": [...]}`` / ``{"not": ...}``) recursively.
    """
    if not isinstance(f, dict):
        raise ValueError(
            f"filter must be a dict, got {type(f).__name__}: {f!r}"
        )
    for key in ("and", "or"):
        if key in f:
            subs = f[key]
            if not isinstance(subs, (list, tuple)) or not subs:
                raise ValueError(
                    f"{key!r} filter expects a non-empty list of filter specs"
                )
            for s in subs:
                _validate_filter(s)
            return
    if "not" in f:
        _validate_filter(f["not"])
        return
    op = f.get("op")
    if op is None and not ("always" in f or "not_field" in f):
        raise ValueError("filter must include an 'op' key")
    compare_ops = {">", "<", ">=", "<=", "==", "!=", "gt", "lt", "ge", "le", "eq", "ne"}
    constant_ops = compare_ops | {
        "starts_with", "ends_with", "contains",
        "strip", "lstrip", "rstrip", "lower", "upper",
        "length", "regex",
    }
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
    if "field" not in f:
        raise ValueError(
            "filter requires either 'field' + 'op' + 'value', or "
            "'field_a' + 'op' + 'field_b'"
        )
    if op == "is_null":
        return
    if op == "is_type":
        if "value" not in f:
            raise ValueError("is_type filter must include a 'value' key")
        return
    if op in ("in", "not_in"):
        if "values" not in f:
            raise ValueError(f"{op} filter must include a 'values' key")
        return
    if "old" in f and "new" in f:
        if "value" not in f:
            raise ValueError("replace filter must include a 'value' key")
        cmp_op = f.get("cmp_op", op)
        if cmp_op not in compare_ops:
            raise ValueError(
                f"unsupported compare op {cmp_op!r}; valid ops: "
                f"> < >= <= == != (or gt lt ge le eq ne)"
            )
        return
    if "value" not in f:
        raise ValueError(
            "filter requires either 'field' + 'op' + 'value', or "
            "'field_a' + 'op' + 'field_b'"
        )
    if op == "regex":
        try:
            re.compile(f["value"])
        except re.error as e:
            raise ValueError(f"invalid regex in filter: {e}") from e
        return
    if op not in constant_ops:
        raise ValueError(
            f"unsupported constant-filter op {op!r}; valid ops: "
            f"> < >= <= == != (or gt lt ge le eq ne), starts_with, ends_with, "
            f"contains, strip, lstrip, rstrip, lower, upper, length, regex"
        )


# Ops that the Rust reader (_crxml_core) does not understand.
# Filter specs using these must fall back to Python execution.
_RUST_UNSUPPORTED_OPS = frozenset({
    "is_null", "is_type", "regex",
    "starts_with", "ends_with", "contains",
    "strip", "lstrip", "rstrip", "lower", "upper", "length",
    "in", "not_in",
})
_RUST_UNSUPPORTED_CMP_OPS = frozenset({
    ">", "<", ">=", "<=", "gt", "lt", "ge", "le",
})


def _filter_needs_python(spec: dict) -> bool:
    """Return True if a filter spec cannot be handled by the Rust reader."""
    if any(k in spec for k in ("or", "and", "not")):
        return True
    op = spec.get("op")
    if op in _RUST_UNSUPPORTED_OPS:
        return True
    if op in _RUST_UNSUPPORTED_CMP_OPS:
        return True
    return False


class CrystalXMLSource(Adapter):
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
        "_engine",
        "_engine_desired",
        "_num_chunks",
        "_memory",
        "_max_split_chunks",
        "_schema_discovered",
    )

    def __init__(
        self,
        source: Union[str, Path],
        *,
        row_tag: str = "Row",
        engine: str = "auto",
        threads: int = 0,
        memory: Optional[Union[str, int]] = None,
        chunks: Optional[int] = None,
        field_mapping: Optional[dict[str, str]] = None,
        drop_fields: Optional[list[str]] = None,
        filter: Optional[dict[str, str]] = None,
        field_types: Optional[dict[str, str]] = None,
        dictionary_columns: Optional[list[str]] = None,
        schema: Optional[list[str]] = None,
        auto_dict: bool = False,
        strict_types: bool = False,
        max_split_chunks: Optional[int] = None,
        observer: Optional[dict] = None,
        use_mmap: bool = True,
        batch_size: int = 1024,
    ):
        # Store adapter-specific kwargs before calling super().__init__
        self._row_tag = row_tag
        self._memory = _parse_memory(memory)
        self._max_split_chunks = chunks if chunks is not None else max_split_chunks

        # Call Source.__init__ for standard kwargs (path, field_mapping, etc.)
        super().__init__(
            source,
            field_mapping=field_mapping,
            drop_fields=drop_fields,
            filter=filter,
            field_types=field_types,
            dictionary_columns=dictionary_columns,
            schema=schema,
            auto_dict=auto_dict,
            strict_types=strict_types,
            observer=observer,
            use_mmap=use_mmap,
            batch_size=batch_size,
        )

        # Validate filter spec eagerly (crxml-specific validation)
        if self._filter is not None:
            _validate_filter(self._filter)

        # Auto-tune: separate optima (Aug 28 sweep, now with frozen schema).
        t = threads if threads > 0 else _default_threads()
        file_bytes = self._path.stat().st_size
        self._num_chunks = max(t, min(16 * t, file_bytes // (4 * 1024 * 1024)))

        self._schema_discovered = bool(self._schema)

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

        size = self._path.stat().st_size
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
        if self._strict_types:
            kwargs["strict_types"] = True
        if self._max_split_chunks is not None:
            kwargs["max_split_chunks"] = self._max_split_chunks
        if self._observer:
            kwargs["observer"] = dict(self._observer)
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
            "strict_types": self._strict_types,
            "max_split_chunks": self._max_split_chunks,
            "observer": dict(self._observer) if self._observer else None,
            "prefault": False,
        }

    def _read_arrow(self, plan_overrides=None):
        if self._cached_arrow is not None and plan_overrides is None:
            return self._cached_arrow
        engine = self._resolve_engine("table")
        plan = self._build_plan_kwargs()
        if plan_overrides:
            from .fusion import _merge_plan_kwargs

            _merge_plan_kwargs(plan, plan_overrides)

        # Check if the filter is something the Rust reader cannot handle:
        # compound specs (or/and/not), or ops like is_null/is_type/regex/>/</etc.
        # If so, fall back to Python execution.
        filter_spec = plan.get("filter")
        needs_python_filter = False
        if filter_spec and isinstance(filter_spec, dict):
            needs_python_filter = _filter_needs_python(filter_spec)
        compound_predicate = None
        if needs_python_filter:
            engine = "stream"
            from .stages.filter import _build_predicate_from_spec
            compound_predicate = _build_predicate_from_spec(filter_spec)
            plan.pop("filter", None)
        # Strip keys that the Rust reader doesn't accept
        plan.pop("max_split_chunks", None)
        plan.pop("observer", None)
        if (
            self._memory is not None
            and self._path.stat().st_size > self._memory
            and _HAS_BOUNDED
        ):
            bounded_kwargs = self._build_bounded_kwargs()
            if plan_overrides:
                from .fusion import _merge_plan_kwargs

                _merge_plan_kwargs(bounded_kwargs, plan_overrides)
            table = _core.read_to_columnar_bounded(
                str(self._path), self._row_tag, self._memory,
                **bounded_kwargs,
            )
        elif engine == "columnar":
            table = _core.read_to_columnar(
                str(self._path), self._row_tag,
                prefault=self._use_mmap, **plan
            )
        elif engine == "parallel":
            table = _core.read_to_columnar_par(
                str(self._path), self._row_tag, self._num_chunks,
                prefault=self._use_mmap, **plan
            )
        else:
            import pyarrow as pa
            rows = []
            for batch in self._iter_batches():
                rows.extend(batch)
            # Apply plan overrides that the Rust reader would normally handle
            field_mapping = plan.get("field_mapping")
            drop_fields = plan.get("drop_fields")
            field_types = plan.get("field_types")
            if field_mapping or drop_fields or field_types or compound_predicate:
                processed = []
                for r in rows:
                    if compound_predicate and not compound_predicate(r):
                        continue
                    if field_mapping:
                        r = {field_mapping.get(k, k): v for k, v in r.items()}
                    if drop_fields:
                        r = {k: v for k, v in r.items() if k not in drop_fields}
                    if field_types:
                        for col_name, type_str in field_types.items():
                            if col_name in r and r[col_name] is not None:
                                if type_str == "int64":
                                    r[col_name] = int(r[col_name])
                                elif type_str == "float64":
                                    r[col_name] = float(r[col_name])
                                elif type_str in ("bool", "boolean"):
                                    r[col_name] = r[col_name] in ("true", "True", "1", "yes")
                    processed.append(r)
                rows = processed
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
        if self._schema:
            return list(self._schema)
        first_row = next(iter(self), None)
        if first_row is None:
            return []
        return [*first_row]

    def _stream_iter(self):
        return CrxmlReader(str(self._path), self._row_tag)

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

    def to_polars(self, memory=None, **kwargs):
        import polars as pl

        if memory is not None:
            chunks = []
            for batch in self.iter_record_batches(memory=memory, **kwargs):
                chunks.append(pl.from_arrow(batch))
            return pl.concat(chunks) if chunks else pl.DataFrame()
        return pl.from_arrow(self.to_arrow())

    def to_pandas(self, memory=None, dtype_backend: str = "pyarrow", **kwargs) -> "pd.DataFrame":
        import pandas as pd

        if memory is not None:
            types_mapper = pd.ArrowDtype if dtype_backend == "pyarrow" else None
            chunks = []
            for batch in self.iter_record_batches(memory=memory, **kwargs):
                chunks.append(batch.to_pandas(types_mapper=types_mapper))
            return pd.concat(chunks, ignore_index=True) if chunks else pd.DataFrame()
        table = self.to_arrow()
        if dtype_backend == "pyarrow":
            return table.to_pandas(types_mapper=pd.ArrowDtype)
        return table.to_pandas()

    def to_parquet(self, path: Union[str, Path], memory=None, **kwargs):
        import pyarrow.parquet as pq

        if memory is not None:
            parquet_keys = {
                "compression", "compression_level", "row_group_size",
                "use_dictionary", "write_statistics",
            }
            parquet_kwargs = {k: v for k, v in kwargs.items() if k in parquet_keys}
            iter_kwargs = {k: v for k, v in kwargs.items() if k not in parquet_keys}
            writer = None
            for batch in self.iter_record_batches(memory=memory, **iter_kwargs):
                if writer is None:
                    writer = pq.ParquetWriter(str(path), batch.schema, **parquet_kwargs)
                writer.write_batch(batch)
            if writer is not None:
                writer.close()
            return
        pq.write_table(self.to_arrow(), str(path), **kwargs)

    def iter_record_batches(
        self, memory: Union[str, int] = "64MiB", batch_size: Optional[int] = None,
        threads: Optional[int] = None, strict: bool = False,
    ) -> Iterator["pa.RecordBatch"]:
        """Yield Arrow ``RecordBatch`` objects with constant memory.

        Unlike ``to_arrow()`` (which materializes a full table) or
        ``iter_batches`` (which materializes then splits), this streams
        directly from Rust via ``BatchConsumer`` and ``StreamingBatchIterator``.
        Peak is ``memory`` + one batch + export buffer: set ``memory="64KB"``
        and ``batch_size=1`` for the smallest footprint (one row per batch,
        ~1 KB for CR rows). Python overhead means true 64 KB is only reachable
        from Rust, but this is still bounded for 50 GB files.

        Parameters
        ----------
        strict:
            When True, the memory budget is a hard limit: oversized batches
            raise ``MemoryError`` instead of being allowed through
            (rypipe-core 0.4.0 strict budget contract). Default False keeps
            the soft-budget behavior.

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
                str(self._path),
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
            str(self._path),
            row_tag=self._row_tag,
            memory=_parse_memory(memory),
            batch_size=batch_size,
            threads=threads,
            strict=strict,
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
