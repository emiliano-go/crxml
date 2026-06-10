from typing import Iterable, Iterator, Callable

_SENTINEL = object()

def validate_stages_picklable(stages):
    import pickle
    for stage in stages:
        try:
            pickle.dumps(stage)
        except Exception as e:
            raise TypeError(f"Stage {stage!r} is not picklable: {e}")

def _reader_thread(source, q, batch_size):
    batch = []
    for rec in source:
        batch.append(rec)
        if len(batch) >= batch_size:
            q.put(batch)
            batch = []
    if batch:
        q.put(batch)
    q.put(_SENTINEL)

def _prefetch_iter(source, batch_size, maxsize=8):
    import queue
    import threading
    q = queue.Queue(maxsize=maxsize)
    t = threading.Thread(target=_reader_thread, args=(source, q, batch_size), daemon=True)
    t.start()
    while True:
        item = q.get()
        if item is _SENTINEL:
            break
        yield from item
    t.join()

def _worker_apply(batch, stages):
    """Worker function – must be module-level for pickling."""
    from .fusion import fused_iter  # re‑import inside worker
    return list(fused_iter(batch, stages))

def _submit_next(raw_stream, batch_size, futures, executor, stages):
    from itertools import islice
    batch = list(islice(raw_stream, batch_size))
    if batch:
        futures.append(executor.submit(_worker_apply, batch, stages))
        return True
    return False

def parallel_iter(
    source: Iterable[dict],
    stages: list[Callable],
    workers: int,
    batch_size: int,
) -> Iterator[dict]:
    from concurrent.futures import ProcessPoolExecutor
    raw_stream = _prefetch_iter(source, batch_size)
    with ProcessPoolExecutor(max_workers=workers) as executor:
        futures = []

        for _ in range(workers * 2):
            if not _submit_next(raw_stream, batch_size, futures, executor, stages):
                break

        idx = 0
        while idx < len(futures):
            yield from futures[idx].result()
            idx += 1
            _submit_next(raw_stream, batch_size, futures, executor, stages)