from typing import Iterable, Iterator, Callable

Stage = Callable[[Iterable[dict]], Iterable[dict]]

class Pipeline:
    __slots__ = ("_source", "_stages", "_batch_size", "_workers")

    def __init__(
        self,
        source: Iterable[dict],
        stages: list[Stage] | None = None,
        *,
        batch_size: int = 1000,
        workers: int | None = None,
    ):
        self._source = source
        self._stages = stages or []
        self._batch_size = batch_size
        self._workers = workers

    def __or__(self, stage: Stage) -> "Pipeline":
        return Pipeline(
            self._source,
            [*self._stages, stage],
            batch_size=self._batch_size,
            workers=self._workers,
        )

    def __iter__(self) -> Iterator[dict]:
        if self._workers:
            from .parallel import parallel_iter, validate_stages_picklable
            validate_stages_picklable(self._stages)
            return parallel_iter(
                self._source,
                self._stages,
                workers=self._workers,
                batch_size=self._batch_size,
            )
        from .fusion import fused_iter
        return fused_iter(self._source, self._stages)

    def _to_arrow(self):
        """Run the whole pipeline as a batch chain to one pyarrow Table.

        Returns None when this pipeline cannot short-circuit to a table
        (worker mode, non-columnar source, or trailing stateful stages);
        callers then fall back to the dict stream.
        """
        if self._workers:
            return None
        src = self._source
        if not (hasattr(src, "_read_arrow") and hasattr(src, "_build_plan_kwargs")):
            return None
        from .batchpipe import build_chain, collect_table
        from .fusion import plan_split

        plan_overrides, remaining = plan_split(self._stages)
        table = src._read_arrow(plan_overrides=plan_overrides or None)
        op, trailing = build_chain(
            table, remaining, batch_size=getattr(src, "_batch_size", 1024)
        )
        if trailing:
            return None
        return collect_table(op)

    def parallel(self, workers: int | None = None, batch_size: int = 1000) -> "Pipeline":
        return Pipeline(
            self._source,
            self._stages,
            batch_size=batch_size,
            workers=workers,
        )

    def iter_record_batches(
        self, memory: int | str = "64MiB", batch_size: int | None = None
    ):
        """Yield ``RecordBatch`` with constant memory.

        Streaming via ``BatchConsumer`` when source and stages are fusable;
        otherwise falls back to ``iter_arrow_batches``.
        """
        src = self._source
        if hasattr(src, "iter_record_batches"):
            try:
                from .fusion import plan_split

                plan_overrides, remaining = plan_split(self._stages)
                if not remaining:
                    yield from src.iter_record_batches(
                        memory=memory, batch_size=batch_size, **(plan_overrides or {})
                    )
                    return
            except Exception:
                pass
        yield from self.iter_arrow_batches(batch_size=batch_size)

    def iter_arrow_batches(self, batch_size: int | None = None):
        """Yield ``RecordBatch`` (materialized fallback)."""
        if batch_size is None:
            batch_size = self._batch_size
        import pyarrow as pa

        table = self._to_arrow()
        if table is not None:
            yield from table.to_batches(max_chunksize=batch_size)
            return
        batch: list[dict] = []
        for row in self:
            batch.append(row)
            if len(batch) >= batch_size:
                yield from pa.Table.from_pylist(batch).to_batches()
                batch = []
        if batch:
            yield from pa.Table.from_pylist(batch).to_batches()
