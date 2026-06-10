class DropFields:
    __slots__ = ("_fields_set",)

    def __init__(self, fields: list[str]):
        self._fields_set = frozenset(fields)

    def apply(self, record: dict) -> dict:
        fields_set = self._fields_set
        if not fields_set:
            return record
        return {k: v for k, v in record.items() if k not in fields_set}

    def __call__(self, stream):
        return map(self.apply, stream)