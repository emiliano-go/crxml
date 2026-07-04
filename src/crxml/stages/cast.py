from typing import Callable

# Mapping from Python built-in type callables to Rust field type strings.
_PY_TO_RUST_TYPE = {
    int: "int64",
    float: "float64",
    str: None,     # string is the default; no-op in the plan
    bool: "bool",
}

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

    def _plan_kwargs(self) -> dict | None:
        """Map standard Python types to BuildPlan field_types.

        Returns None when any cast target is a non-standard callable
        (lambda, custom function) — those must stay on the dict path.
        """
        ft = {}
        for field, fn in self._mapping.items():
            rust_type = _PY_TO_RUST_TYPE.get(fn)
            if rust_type is None:
                if fn is str:
                    continue  # no-op; skip
                return None  # non-fusable custom callable
            ft[field] = rust_type
        if not ft:
            return None
        return {"field_types": ft}