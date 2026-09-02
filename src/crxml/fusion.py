from typing import Iterable, Iterator, Callable


def plan_split(stages):
    """Split stages into (BuildPlan pushdown kwargs, remaining stages)."""
    plan_overrides = {}
    remaining = []
    for stage in stages:
        if hasattr(stage, "_plan_kwargs"):
            kwargs = stage._plan_kwargs()
            if kwargs is not None:
                plan_overrides.update(kwargs)
                continue
        remaining.append(stage)
    return plan_overrides, remaining


def _try_columnar_fusion(source, stages):
    if not hasattr(source, "_read_arrow") or not hasattr(source, "_build_plan_kwargs"):
        return None

    plan_overrides, remaining = plan_split(stages)

    if not plan_overrides and len(remaining) == len(stages):
        return None

    table = source._read_arrow(plan_overrides=plan_overrides or None)
    # Remaining stages run through the vectorized batch chain instead of
    # row-at-a-time dict reconstruction; only trailing generic (possibly
    # stateful) stream stages fall back to the dict stream.
    from .batchpipe import build_chain, iter_dicts

    op, trailing = build_chain(
        table, remaining, batch_size=getattr(source, "_batch_size", 1024)
    )
    stream = iter_dicts(op)
    for stage in trailing:
        stream = stage(stream)
    return stream


def is_fusable(stage) -> bool:
    try:
        return callable(stage.apply)
    except AttributeError:
        return False


def fused_iter(source: Iterable[dict], stages: list[Callable]) -> Iterator[dict]:
    result = _try_columnar_fusion(source, stages)
    if result is not None:
        return result

    fusables = []
    rem = list(stages)
    while rem and is_fusable(rem[0]):
        fusables.append(rem.pop(0))

    bound = [s.apply for s in fusables]

    source_iter = (
        source._iter_batches()
        if hasattr(source, "_iter_batches")
        else source
    )

    # No fusable stages: don't wrap the source in a pass-through generator,
    # it costs one generator dispatch per row for nothing.
    if not bound:
        stream = (
            (r for batch in source_iter for r in batch)
            if hasattr(source, "_iter_batches")
            else iter(source_iter)
        )
        for stage in rem:
            stream = stage(stream)
        return stream

    def fused():
        iterator = (
            (r for batch in source_iter for r in batch)
            if hasattr(source, "_iter_batches")
            else source_iter
        )
        for record in iterator:
            r = record
            for fn in bound:
                r = fn(r)
                if r is None:
                    break
            else:
                yield r

    stream = fused()
    for stage in rem:
        stream = stage(stream)
    return stream
