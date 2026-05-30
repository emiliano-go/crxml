class RenameFields:
    def __init__(self, mapping: dict[str, str]):
        self._mapping = mapping

    def apply(self, record: dict) -> dict:
        return {self._mapping.get(k, k): v for k, v in record.items()}

    def __call__(self, stream):
        return map(self.apply, stream)