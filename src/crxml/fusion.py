from typing import Iterable, Iterator, Callable, Protocol

class Fusable(Protocol):
    def apply(self, record: dict) -> dict | None: ...

def is_fusable(stage) -> bool:
    return hasattr(stage, "apply") and callable(stage.apply)

def fused_iter(source: Iterable[dict], stages: list[Callable]) -> Iterator[dict]:
    # Split into leading fusables and the rest
    fusables = []
    rem = list(stages)
    while rem and is_fusable(rem[0]):
        fusables.append(rem.pop(0))

    # Fused part
    def fused():
        for record in source:
            r = record
            for stage in fusables:
                r = stage.apply(r)
                if r is None:
                    break
            else:
                yield r

    stream = fused()
    for stage in rem:
        stream = stage(stream)
    return stream