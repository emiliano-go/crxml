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
    try:
        batch = []
        for rec in source:
            batch.append(rec)
            if len(batch) >= batch_size:
                q.put(batch)
                batch = []
        if batch:
            q.put(batch)
    except BaseException as e:
        q.put(e)  # propagate to consumer instead of hanging it
    finally:
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
        if isinstance(item, BaseException):
            raise item
        yield from item
    t.join()

def _worker_apply(batch, stages):
    """Worker function; must be module-level for pickling."""
    from .fusion import fused_iter  # re-import inside worker
    return list(fused_iter(batch, stages))

def parallel_iter(
    source: Iterable[dict],
    stages: list[Callable],
    workers: int,
    batch_size: int,
) -> Iterator[dict]:
    """Run stages over row batches in worker processes.

    In-flight work is capped at ``workers * 2`` batches, so memory stays
    bounded regardless of input size.
    """
    from collections import deque
    from concurrent.futures import ProcessPoolExecutor
    raw_stream = _prefetch_iter(source, batch_size)
    with ProcessPoolExecutor(max_workers=workers) as executor:
        window: deque = deque()

        def submit() -> bool:
            from itertools import islice
            batch = list(islice(raw_stream, batch_size))
            if not batch:
                return False
            window.append(executor.submit(_worker_apply, batch, stages))
            return True

        for _ in range(workers * 2):
            if not submit():
                break
        while window:
            yield from window.popleft().result()
            submit()
