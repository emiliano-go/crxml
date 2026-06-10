from typing import Iterable, Iterator, Callable

Stage = Callable[[Iterable[dict]], Iterable[dict]]

class Pipeline:
    __slots__ = ("_source", "_stages", "_batch_size", "_prefetch", "_workers")

    def __init__(
        self,
        source: Iterable[dict],
        stages: list[Stage] | None = None,
        *,
        batch_size: int = 1000,
        prefetch: bool = False,
        workers: int | None = None,
    ):
        self._source = source
        self._stages = stages or []
        self._batch_size = batch_size
        self._prefetch = prefetch
        self._workers = workers

    def __or__(self, stage: Stage) -> "Pipeline":
        return Pipeline(
            self._source,
            [*self._stages, stage],
            batch_size=self._batch_size,
            prefetch=self._prefetch,
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

    def parallel(self, workers: int | None = None, batch_size: int = 1000) -> "Pipeline":
        return Pipeline(
            self._source,
            self._stages,
            batch_size=batch_size,
            prefetch=self._prefetch,
            workers=workers,
        )