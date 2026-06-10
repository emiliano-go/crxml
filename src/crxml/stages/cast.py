from typing import Callable

class CastTypes:
    __slots__ = ("_mapping",)

    def __init__(self, mapping: dict[str, Callable]):
        self._mapping = mapping

    def apply(self, record: dict) -> dict:
        mapping = self._mapping
        if not mapping:
            return record
        out = record.copy()
        for field, cast_fn in mapping.items():
            try:
                out[field] = cast_fn(out[field])
            except KeyError:
                pass
            except (ValueError, TypeError) as e:
                val = out[field]
                raise ValueError(
                    f"CastTypes: cannot cast field '{field}' "
                    f"value {val!r} — {e}"
                ) from e
        return out

    def __call__(self, stream):
        return map(self.apply, stream)