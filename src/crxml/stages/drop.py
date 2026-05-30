class DropFields:
    def __init__(self, fields: list[str]):
        self._fields_set = frozenset(fields)

    def apply(self, record: dict) -> dict:
        return {k: v for k, v in record.items() if k not in self._fields_set}

    def __call__(self, stream):
        return map(self.apply, stream)