class FilterRows:
    __slots__ = ("_predicate",)

    def __init__(self, predicate):
        self._predicate = predicate

    def apply(self, record: dict) -> dict | None:
        return record if self._predicate(record) else None

    def __call__(self, stream):
        return filter(None, map(self.apply, stream))