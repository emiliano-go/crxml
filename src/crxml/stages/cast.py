from typing import Callable

class CastTypes:
    def __init__(self, mapping: dict[str, Callable]):
        self._mapping = mapping

    def apply(self, record: dict) -> dict:
        out = record.copy()
        for field, cast_fn in self._mapping.items():
            if field in out:
                try:
                    out[field] = cast_fn(out[field])
                except (ValueError, TypeError) as e:
                    raise ValueError(
                        f"CastTypes: cannot cast field '{field}' "
                        f"value {out[field]!r} — {e}"
                    ) from e
        return out

    def __call__(self, stream):
        return map(self.apply, stream)