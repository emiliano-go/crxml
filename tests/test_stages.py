"""Unit tests for all stages and pipeline internals (no file I/O)."""
import pickle
import pytest
from crxml import Pipeline, RenameFields, CastTypes, DropFields, FilterRows
from crxml.pipeline import Pipeline as PipelineCls
from crxml.fusion import fused_iter, is_fusable


# ── Stage constructors ────────────────────────────────────────────

class TestRenameFields:
    def test_rename_all_keys(self, sample_rows):
        stage = RenameFields({"name": "full_name", "age": "years"})
        result = list(stage(iter(sample_rows)))
        assert result[0] == {"full_name": "Alice", "years": "30", "city": "NYC"}

    def test_rename_subset(self, sample_rows):
        stage = RenameFields({"name": "full_name"})
        result = list(stage(iter(sample_rows)))
        assert result[0] == {"full_name": "Alice", "age": "30", "city": "NYC"}

    def test_rename_nonexistent_key(self, sample_rows):
        stage = RenameFields({"missing": "x"})
        result = list(stage(iter(sample_rows)))
        assert result[0] == {"name": "Alice", "age": "30", "city": "NYC"}

    def test_empty_mapping(self, sample_rows):
        stage = RenameFields({})
        result = list(stage(iter(sample_rows)))
        assert result == sample_rows

    def test_rename_preserves_order(self, sample_rows):
        stage = RenameFields({"age": "years"})
        result = list(stage(iter(sample_rows)))
        keys = list(result[0].keys())
        assert keys == ["name", "years", "city"]


class TestCastTypes:
    def test_cast_int(self, sample_rows):
        stage = CastTypes({"age": int})
        result = list(stage(iter(sample_rows)))
        assert isinstance(result[0]["age"], int)
        assert result[0]["age"] == 30

    def test_cast_float(self, sample_rows):
        rows = [{"val": "3.14"}, {"val": "2.71"}]
        stage = CastTypes({"val": float})
        result = list(stage(iter(rows)))
        assert result[0]["val"] == 3.14
        assert result[1]["val"] == 2.71

    def test_cast_raises_on_bad_value(self):
        rows = [{"val": "not_a_number"}]
        stage = CastTypes({"val": float})
        with pytest.raises(ValueError, match="cannot cast"):
            list(stage(iter(rows)))

    def test_cast_raises_on_bad_value_custom_message(self):
        rows = [{"val": "abc"}]
        stage = CastTypes({"val": int})
        with pytest.raises(ValueError):
            list(stage(iter(rows)))

    def test_cast_skips_missing_field(self, sample_rows):
        stage = CastTypes({"nonexistent": int})
        result = list(stage(iter(sample_rows)))
        assert result == sample_rows

    def test_cast_empty_mapping(self, sample_rows):
        stage = CastTypes({})
        result = list(stage(iter(sample_rows)))
        assert result == sample_rows

    def test_cast_str_to_str(self, sample_rows):
        stage = CastTypes({"name": str})
        result = list(stage(iter(sample_rows)))
        assert isinstance(result[0]["name"], str)


class TestDropFields:
    def test_drop_single_field(self, sample_rows):
        stage = DropFields(["age"])
        result = list(stage(iter(sample_rows)))
        assert "age" not in result[0]
        assert list(result[0].keys()) == ["name", "city"]

    def test_drop_multiple_fields(self, sample_rows):
        stage = DropFields(["age", "city"])
        result = list(stage(iter(sample_rows)))
        assert list(result[0].keys()) == ["name"]

    def test_drop_nonexistent_field(self, sample_rows):
        stage = DropFields(["missing"])
        result = list(stage(iter(sample_rows)))
        assert result == sample_rows

    def test_drop_all_fields(self, sample_rows):
        stage = DropFields(["name", "age", "city"])
        result = list(stage(iter(sample_rows)))
        assert result[0] == {}

    def test_drop_empty_list(self, sample_rows):
        stage = DropFields([])
        result = list(stage(iter(sample_rows)))
        assert result == sample_rows


class TestFilterRows:
    def test_filter_keeps_all(self, sample_rows):
        stage = FilterRows(lambda r: True)
        result = list(stage(iter(sample_rows)))
        assert result == sample_rows

    def test_filter_drops_all(self, sample_rows):
        stage = FilterRows(lambda r: False)
        result = list(stage(iter(sample_rows)))
        assert result == []

    def test_filter_predicate(self, sample_rows):
        stage = FilterRows(lambda r: int(r["age"]) > 30)
        result = list(stage(iter(sample_rows)))
        assert len(result) == 1
        assert result[0]["name"] == "Carol"

    def test_filter_check_missing_key_returns_false(self, sample_rows):
        stage = FilterRows(lambda r: r.get("missing") == "x")
        result = list(stage(iter(sample_rows)))
        assert result == []

    def test_filter_preserves_dict_identity(self, sample_rows):
        stage = FilterRows(lambda r: r["name"] == "Alice")
        result = list(stage(iter(sample_rows)))
        assert result[0] is sample_rows[0]


# ── Fusable protocol ─────────────────────────────────────────────

class TestFusable:
    """Custom fusable stages must implement both apply() and __call__()."""

    def test_stages_are_fusable(self):
        assert is_fusable(RenameFields({"a": "b"}))
        assert is_fusable(CastTypes({"a": int}))
        assert is_fusable(DropFields(["a"]))
        assert is_fusable(FilterRows(lambda r: True))

    def test_lambda_not_fusable(self):
        assert not is_fusable(lambda x: x)

    def test_generator_not_fusable(self):
        def gen(stream):
            yield from stream
        assert not is_fusable(gen)

    def test_custom_fusable_class(self):
        class Double:
            def apply(self, record):
                record["x"] = record.get("x", 0) * 2
                return record
            def __call__(self, stream):
                return map(self.apply, stream)
        assert is_fusable(Double())

    def test_custom_non_fusable_class(self):
        class NoApply:
            def __call__(self, stream):
                return stream
        assert not is_fusable(NoApply())


# ── Fusion ───────────────────────────────────────────────────────

class TestFusion:
    def test_fused_single_stage(self, sample_rows):
        stages = [RenameFields({"name": "full_name"})]
        result = list(fused_iter(iter(sample_rows), stages))
        assert "full_name" in result[0]
        assert "name" not in result[0]

    def test_fused_multiple_stages(self, sample_rows):
        stages = [
            RenameFields({"name": "full_name"}),
            DropFields(["age"]),
        ]
        result = list(fused_iter(iter(sample_rows), stages))
        assert list(result[0].keys()) == ["full_name", "city"]

    def test_fused_filter_drops_row(self, sample_rows):
        stages = [FilterRows(lambda r: r["name"] != "Bob")]
        result = list(fused_iter(iter(sample_rows), stages))
        assert len(result) == 2
        assert result[0]["name"] == "Alice"

    def test_fused_cast_then_filter(self, sample_rows):
        stages = [
            CastTypes({"age": int}),
            FilterRows(lambda r: r["age"] >= 30),
        ]
        result = list(fused_iter(iter(sample_rows), stages))
        assert len(result) == 2
        assert result[0]["name"] == "Alice"

    def test_mixed_fusable_and_non_fusable(self, sample_rows):
        stages = [
            RenameFields({"name": "full_name"}),    # fusable
            lambda stream: map(lambda r: {**r, "seen": True}, stream),  # non-fusable
            DropFields(["city"]),                    # fusable
        ]
        result = list(fused_iter(iter(sample_rows), stages))
        assert "full_name" in result[0]
        assert "seen" in result[0]
        assert "city" not in result[0]

    def test_no_stages(self, sample_rows):
        result = list(fused_iter(iter(sample_rows), []))
        assert result == sample_rows


# ── Picklability ─────────────────────────────────────────────────

class TestPicklability:
    def test_rename_fields_picklable(self):
        stage = RenameFields({"a": "b"})
        data = pickle.dumps(stage)
        restored = pickle.loads(data)
        assert restored._mapping == {"a": "b"}

    def test_cast_types_picklable(self):
        stage = CastTypes({"a": int})
        data = pickle.dumps(stage)
        restored = pickle.loads(data)
        assert restored._mapping == {"a": int}

    def test_drop_fields_picklable(self):
        stage = DropFields(["a"])
        data = pickle.dumps(stage)
        restored = pickle.loads(data)
        assert restored._fields_set == frozenset(["a"])

    def test_filter_rows_lambda_not_picklable(self):
        stage = FilterRows(lambda r: True)
        with pytest.raises((pickle.PicklingError, AttributeError)):
            pickle.dumps(stage)

    def test_filter_rows_module_function_picklable(self):
        stage = FilterRows(_keep_all)
        data = pickle.dumps(stage)
        restored = pickle.loads(data)
        assert restored._predicate is _keep_all


def _keep_all(r):
    return True
