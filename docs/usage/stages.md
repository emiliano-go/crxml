# Built-in Stages

## RenameFields

```python
RenameFields(mapping: dict[str, str])
```

Renames dict keys according to `mapping`. Unmapped keys pass through unchanged.

**Example:**

```python
# Input:  {"{Report.Vendor}": "Acme", "{Report.Price}": "50.00"}
# Output: {"supplier": "Acme", "cost": "50.00"}

RenameFields({"{Report.Vendor}": "supplier", "{Report.Price}": "cost"})
```

## CastTypes

```python
CastTypes(mapping: dict[str, type])
```

Casts specified fields to the given types. Raises `ValueError` if a cast
fails.

**Example:**

```python
# Input:  {"invoice": "INV-001", "qty": "3", "price": "19.99"}
# Output: {"invoice": "INV-001", "qty": 3, "price": 19.99}

CastTypes({"qty": int, "price": float})
```

## DropFields

```python
DropFields(fields: list[str])
```

Removes specified keys from each row.

**Example:**

```python
# Input:  {"a": "1", "b": "2", "c": "3"}
# Output: {"a": "1", "c": "3"}

DropFields(["b"])
```

## FilterRows

```python
FilterRows(predicate: Callable[[dict[str, Any]], bool])
```

Keeps only rows where `predicate(row)` returns `True`.

**Example:**

```python
# Input:  [{"amt": "10"}, {"amt": "200"}, {"amt": "50"}]
# Output: [{"amt": "200"}]

FilterRows(lambda r: float(r.get("amt", 0)) > 100)
```

## Edge cases

- **RenameFields:** If a mapping key does not exist in the row, it is silently
  ignored. Duplicate target names are not checked, the last mapping wins.
- **CastTypes:** Raises `ValueError` if a value cannot be cast to the target type.
- **DropFields:** Dropping a non-existent key is a no-op.
- **FilterRows:** The predicate receives the row *after* all prior stages.
