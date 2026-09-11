import re


class _ConstantPredicate:
    __slots__ = ("_field", "_op", "_value", "_compiled")

    _VALID_OPS = frozenset({
        "==", "eq", "!=", "ne",
        ">", "gt", "<", "lt", ">=", "ge", "<=", "le",
        "regex", "starts_with", "ends_with", "contains",
    })

    _CMP_FNS = {
        ">": lambda a, b: a > b,
        "<": lambda a, b: a < b,
        ">=": lambda a, b: a >= b,
        "<=": lambda a, b: a <= b,
        "==": lambda a, b: a == b,
        "!=": lambda a, b: a != b,
        "eq": lambda a, b: a == b,
        "ne": lambda a, b: a != b,
        "gt": lambda a, b: a > b,
        "lt": lambda a, b: a < b,
        "ge": lambda a, b: a >= b,
        "le": lambda a, b: a <= b,
    }

    def __init__(self, field: str, op: str, value: str):
        if op not in self._VALID_OPS:
            raise ValueError(
                f"FilterRows: unsupported operator {op!r} for constant filter; "
                f"valid operators: {' '.join(sorted(self._VALID_OPS))}"
            )
        self._field = field
        self._op = op
        self._value = value
        self._compiled = re.compile(value) if op == "regex" else None

    def __call__(self, record: dict) -> bool:
        actual = record.get(self._field)
        if self._op in ("==", "eq"):
            return actual == self._value
        if self._op in ("!=", "ne"):
            return actual != self._value
        if actual is None:
            return False
        if self._op == "regex":
            return bool(self._compiled.search(str(actual)))
        if self._op == "starts_with":
            return str(actual).startswith(self._value)
        if self._op == "ends_with":
            return str(actual).endswith(self._value)
        if self._op == "contains":
            return self._value in str(actual)
        fn = self._CMP_FNS.get(self._op)
        if fn:
            return fn(actual, self._value)
        return False


class _ComparePredicate:
    __slots__ = ("_field_a", "_op", "_field_b")

    _OPS = {
        ">": lambda a, b: a > b,
        "<": lambda a, b: a < b,
        ">=": lambda a, b: a >= b,
        "<=": lambda a, b: a <= b,
        "==": lambda a, b: a == b,
        "!=": lambda a, b: a != b,
        "eq": lambda a, b: a == b,
        "ne": lambda a, b: a != b,
        "gt": lambda a, b: a > b,
        "lt": lambda a, b: a < b,
        "ge": lambda a, b: a >= b,
        "le": lambda a, b: a <= b,
    }

    def __init__(self, field_a: str, op: str, field_b: str):
        if op not in self._OPS:
            valid = ", ".join(sorted(self._OPS))
            raise ValueError(
                f"FilterRows: unsupported operator {op!r} for column comparison; "
                f"valid operators: {valid}"
            )
        self._field_a = field_a
        self._op = op
        self._field_b = field_b

    def __call__(self, record: dict) -> bool:
        fn = self._OPS.get(self._op)
        return bool(fn(record.get(self._field_a), record.get(self._field_b)))


class _IsNullPredicate:
    __slots__ = ("_field",)

    def __init__(self, field: str):
        self._field = field

    def __call__(self, record: dict) -> bool:
        return record.get(self._field) is None


class _NotNullPredicate:
    __slots__ = ("_field",)

    def __init__(self, field: str):
        self._field = field

    def __call__(self, record: dict) -> bool:
        return record.get(self._field) is not None


class _RegexPredicate:
    """Fusable predicate: regex search against str(r["field"])"""
    __slots__ = ("_field", "_value", "_compiled")

    def __init__(self, field: str, value: str):
        self._field = field
        self._value = value
        self._compiled = re.compile(value)

    def __call__(self, record: dict) -> bool:
        actual = record.get(self._field)
        if actual is None:
            return False
        return bool(self._compiled.search(str(actual)))


_CMP_OPS = {
    ">": lambda a, b: a > b,
    "<": lambda a, b: a < b,
    ">=": lambda a, b: a >= b,
    "<=": lambda a, b: a <= b,
    "==": lambda a, b: a == b,
    "!=": lambda a, b: a != b,
    "eq": lambda a, b: a == b,
    "ne": lambda a, b: a != b,
    "gt": lambda a, b: a > b,
    "lt": lambda a, b: a < b,
    "ge": lambda a, b: a >= b,
    "le": lambda a, b: a <= b,
}


def _build_predicate_from_spec(spec: dict):
    """Build a predicate callable from a filter spec dict (used for Python fallback)."""
    if "always" in spec:
        val = spec["always"]
        return lambda r: val
    if "field" in spec and "op" in spec:
        op = spec["op"]
        if op == "is_null":
            return _IsNullPredicate(spec["field"])
        if op == "is_type":
            return _IsTypePredicate(spec["field"], spec["value"])
        if "value" in spec:
            if op == "regex":
                return _RegexPredicate(spec["field"], spec["value"])
            if op == "starts_with":
                field, value = spec["field"], spec["value"]
                return lambda r: str(r.get(field, "")).startswith(value)
            if op == "ends_with":
                field, value = spec["field"], spec["value"]
                return lambda r: str(r.get(field, "")).endswith(value)
            if op == "contains":
                field, value = spec["field"], spec["value"]
                return lambda r: value in str(r.get(field, ""))
            cmp_fn = _CMP_OPS.get(op)
            if cmp_fn is None:
                return None
            field, value = spec["field"], spec["value"]
            return lambda r: r.get(field) is not None and cmp_fn(r.get(field), value)
        if "values" in spec:
            field, values = spec["field"], tuple(spec["values"])
            if op == "not_in":
                return lambda r: r.get(field) not in values
            return lambda r: r.get(field) in values
        return None
    if "field_a" in spec and "op" in spec and "field_b" in spec:
        return _ComparePredicate(spec["field_a"], spec["op"], spec["field_b"])
    if "and" in spec:
        predicates = [_build_predicate_from_spec(s) for s in spec["and"]]
        if any(p is None for p in predicates):
            return None
        return lambda r: all(p(r) for p in predicates)
    if "or" in spec:
        predicates = [_build_predicate_from_spec(s) for s in spec["or"]]
        if any(p is None for p in predicates):
            return None
        return lambda r: any(p(r) for p in predicates)
    if "not" in spec:
        inner = _build_predicate_from_spec(spec["not"])
        if inner is None:
            return None
        return lambda r: not inner(r)
    return None


class _IsTypePredicate:
    __slots__ = ("_field", "_field_type")

    _VALID_TYPES = frozenset({
        "string", "int64", "float64", "bool", "boolean",
        "dictionary", "date32", "timestamp", "decimal128",
    })

    def __init__(self, field: str, field_type: str):
        if field_type.lower() not in self._VALID_TYPES:
            raise ValueError(
                f"FilterRows: unsupported type {field_type!r}; "
                f"valid types: {', '.join(sorted(self._VALID_TYPES))}"
            )
        self._field = field
        self._field_type = field_type.lower()

    def __call__(self, record: dict) -> bool:
        val = record.get(self._field)
        if val is None:
            return False
        if self._field_type == "int64":
            return isinstance(val, int) and not isinstance(val, bool)
        elif self._field_type == "float64":
            return isinstance(val, float)
        elif self._field_type in ("bool", "boolean"):
            return isinstance(val, bool)
        elif self._field_type in ("string", "dictionary"):
            return isinstance(val, str)
        elif self._field_type in ("date32", "timestamp"):
            return isinstance(val, (int, str))
        elif self._field_type == "decimal128":
            return isinstance(val, (int, float, str))
        return False


class FilterRows:
    """Pipeline stage that filters records by a predicate.

    Accepts a callable predicate, an expression predicate built with
    ``rypipe.expr.col`` (fusable into the Rust parse loop), or declarative
    filter arguments: a constant comparison (``field``, ``op``, ``value``),
    a column-vs-column comparison (``field_a``, ``op``, ``field_b``), a null
    check (``field``, ``is_null=True``), or a type check (``field``,
    ``is_type="..."``).
    """
    __slots__ = ("_predicate", "_filter_spec")

    def __init__(self, predicate=None, *, field=None, op=None, value=None, field_a=None, field_b=None,
                 is_null=None, is_type=None):
        if predicate is not None:
            to_spec = getattr(predicate, "_to_spec", None)
            if callable(to_spec):
                # Expression API predicate (rypipe.expr.Predicate); fusable
                spec = to_spec()
                self._filter_spec = spec
                self._predicate = _build_predicate_from_spec(spec)
                if self._predicate is None:
                    raise ValueError(
                        f"FilterRows: cannot build a predicate from spec {spec!r}"
                    )
            else:
                # Plain callable; Python fallback execution, not fusable
                self._predicate = predicate
                self._filter_spec = None
        elif is_null is not None and field is not None:
            if is_null:
                self._filter_spec = {"field": field, "op": "is_null"}
                self._predicate = _IsNullPredicate(field)
            else:
                self._filter_spec = {"not": {"field": field, "op": "is_null"}}
                self._predicate = _NotNullPredicate(field)
        elif is_type is not None and field is not None:
            self._filter_spec = {"field": field, "op": "is_type", "value": is_type}
            self._predicate = _IsTypePredicate(field, is_type)
        elif field is not None and op is not None and value is not None:
            self._filter_spec = {"field": field, "op": op, "value": value}
            if op == "regex":
                self._predicate = _RegexPredicate(field, value)
            else:
                self._predicate = _ConstantPredicate(field, op, value)
        elif field_a is not None and op is not None and field_b is not None:
            self._filter_spec = {"field_a": field_a, "op": op, "field_b": field_b}
            self._predicate = _ComparePredicate(field_a, op, field_b)
        else:
            raise ValueError(
                "FilterRows requires either a callable predicate, an "
                "expression predicate (see rypipe.expr), or "
                "keyword arguments (field+op+value for constant filter, "
                "field_a+op+field_b for column comparison, "
                "field+is_null=True for a null check or field+is_null=False "
                "for a not-null check, or field+is_type for type check). "
                f"Got predicate={predicate!r}, field={field!r}, op={op!r}, "
                f"value={value!r}, field_a={field_a!r}, field_b={field_b!r}, "
                f"is_null={is_null!r}, is_type={is_type!r}"
            )

    def apply(self, record: dict) -> dict | None:
        return record if self._predicate(record) else None

    def __call__(self, stream):
        # Identity check, not truthiness: a row that earlier stages reduced
        # to {} is still a row; filter(None, ...) would drop it.
        return (r for r in map(self.apply, stream) if r is not None)

    def _plan_kwargs(self) -> dict | None:
        if self._filter_spec is not None:
            return {"filter": self._filter_spec}
        return None


def _require_filter_spec(obj, label: str) -> dict:
    """Extract a fusable spec or raise with a helpful message."""
    if isinstance(obj, FilterRows):
        if obj._filter_spec is None:
            raise ValueError(
                f"{label} only accepts fusable filters: FilterRows with field/op/value, "
                f"field_a/op/field_b, is_null, or is_type keyword form, or an expression "
                f"predicate (col(...)). Plain lambdas/Callables cannot be combined."
            )
        return obj._filter_spec
    if isinstance(obj, (FilterRowsAny, FilterRowsAll, FilterRowsNot)):
        return obj._combined_spec()
    raise TypeError(
        f"{label} expects FilterRows or combinator instances, got {type(obj).__name__!r}"
    )


def _matches(obj, record: dict) -> bool:
    """Uniform row test for FilterRows and combinators."""
    if isinstance(obj, FilterRows):
        return obj._predicate(record)
    return obj.apply(record) is not None


class FilterRowsAny:
    """Keep rows that satisfy **any** of the given fusable filters (OR).

    Each argument must be a :class:`FilterRows` built with the keyword form
    so it can be pushed into the Rust parse loop.

    Example::

        FilterRowsAny(
            FilterRows(field="Department", op="==", value="Sales"),
            FilterRows(field="Status", op="==", value="Inactive"),
        )
    """

    __slots__ = ("_filters", "_specs")

    def __init__(self, *filters: FilterRows):
        if len(filters) < 2:
            raise ValueError("FilterRowsAny requires at least two filters")
        self._filters = filters
        self._specs = [_require_filter_spec(f, "FilterRowsAny") for f in filters]

    def apply(self, record: dict) -> dict | None:
        for f in self._filters:
            if _matches(f, record):
                return record
        return None

    def __call__(self, stream):
        return (r for r in map(self.apply, stream) if r is not None)

    def _combined_spec(self) -> dict:
        return {"or": self._specs}

    def _plan_kwargs(self) -> dict | None:
        return {"filter": self._combined_spec()}


class FilterRowsAll:
    """Keep rows that satisfy **all** of the given fusable filters (AND).

    Chaining plain ``FilterRows`` stages with ``|`` already implies AND; this
    class makes an explicit conjunction useful inside another combinator.

    Example::

        FilterRowsAll(
            FilterRows(field="Status", op="==", value="Active"),
            FilterRows(field="Department", op="==", value="Sales"),
        )
    """

    __slots__ = ("_filters", "_specs")

    def __init__(self, *filters: FilterRows):
        if len(filters) < 2:
            raise ValueError("FilterRowsAll requires at least two filters")
        self._filters = filters
        self._specs = [_require_filter_spec(f, "FilterRowsAll") for f in filters]

    def apply(self, record: dict) -> dict | None:
        for f in self._filters:
            if not _matches(f, record):
                return None
        return record

    def __call__(self, stream):
        return (r for r in map(self.apply, stream) if r is not None)

    def _combined_spec(self) -> dict:
        return {"and": self._specs}

    def _plan_kwargs(self) -> dict | None:
        return {"filter": self._combined_spec()}


class FilterRowsNot:
    """Negate a single fusable filter.

    Example::

        FilterRowsNot(FilterRows(field="Status", op="==", value="Active"))
        # keeps rows where Status is not 'Active'
    """

    __slots__ = ("_inner", "_spec")

    def __init__(self, inner: FilterRows):
        self._inner = inner
        self._spec = _require_filter_spec(inner, "FilterRowsNot")

    def apply(self, record: dict) -> dict | None:
        return None if _matches(self._inner, record) else record

    def __call__(self, stream):
        return (r for r in map(self.apply, stream) if r is not None)

    def _combined_spec(self) -> dict:
        return {"not": self._spec}

    def _plan_kwargs(self) -> dict | None:
        return {"filter": self._combined_spec()}
