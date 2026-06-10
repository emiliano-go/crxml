from typing import Iterable, Iterator, Callable

def is_fusable(stage) -> bool:
    try:
        return callable(stage.apply)
    except AttributeError:
        return False

def fused_iter(source: Iterable[dict], stages: list[Callable]) -> Iterator[dict]:
    # Split into leading fusables and the rest
    fusables = []
    rem = list(stages)
    while rem and is_fusable(rem[0]):
        fusables.append(rem.pop(0))

    # Fused part
    bound = [s.apply for s in fusables]

    def fused():
        for record in source:
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