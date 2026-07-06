use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use quick_xml::events::Event;
use quick_xml::Reader;
// Fx-hashed maps: field-name dispatch is a per-field, per-row lookup;
// SipHash showed at ~7-9% CPU in VTune. Aliased so the rest of the file
// keeps the familiar names.
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::io::Cursor;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BooleanArray, DictionaryArray, Float64Array, Int32Array, Int64Array,
    StringArray,
};
use arrow::datatypes::{DataType, Field as ArrowField, Int32Type, Schema};
use arrow::pyarrow::ToPyArrow;
use arrow::record_batch::RecordBatch;

/// The storage type for a column.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldType {
    String,
    Int64,
    Float64,
    Boolean,
    Dictionary,
}

impl FieldType {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "string" => Some(FieldType::String),
            "int64" => Some(FieldType::Int64),
            "float64" => Some(FieldType::Float64),
            "bool" | "boolean" => Some(FieldType::Boolean),
            "dictionary" => Some(FieldType::Dictionary),
            _ => None,
        }
    }
}

/// A compiled build plan that controls field renaming, dropping,
/// type assignment, dictionary encoding, row filtering (per-row
/// and post-reduce), and column ordering during parse.
/// Default (empty) is a no-op.
#[derive(Clone, Debug)]
pub struct BuildPlan {
    /// Map from raw XML field name to output column name.
    pub field_map: HashMap<String, String>,
    /// Set of raw XML field names to drop entirely.
    pub drop_fields: HashSet<String>,
    /// Explicit type overrides per output column name.
    pub field_types: HashMap<String, FieldType>,
    /// Set of output column names to dict-encode.
    pub dictionary_columns: HashSet<String>,
    /// Optional row filter predicate.
    pub filter: Option<FilterPredicate>,
    /// Desired output column order (names in order).  Columns not
    /// listed here appear after all listed columns in first-appearance
    /// order.  If empty, first-appearance order is used.
    pub schema_order: Vec<String>,
    /// When true, string columns with low cardinality are automatically
    /// upgraded to dictionary encoding during parse.
    pub auto_dict: bool,
}

impl BuildPlan {
    pub fn new() -> Self {
        BuildPlan {
            field_map: HashMap::default(),
            drop_fields: HashSet::default(),
            field_types: HashMap::default(),
            dictionary_columns: HashSet::default(),
            filter: None,
            schema_order: Vec::new(),
            auto_dict: false,
        }
    }

    /// Determine the storage type for an output column name.
    pub fn column_type(&self, name: &str) -> FieldType {
        if let Some(ft) = self.field_types.get(name) {
            return ft.clone();
        }
        if self.dictionary_columns.contains(name) {
            return FieldType::Dictionary;
        }
        FieldType::String
    }

    /// Resolve a raw field name to its output column name.
    /// Returns `None` if the field should be dropped.
    ///
    /// Application order: rename first, then drop — matching left-to-right
    /// pipeline semantics (a rename changes the field name before the drop
    /// check, so a drop targets the renamed name, not the original).
    pub fn resolve_field<'a>(&'a self, raw: &'a str) -> Option<&'a str> {
        let resolved = self.field_map.get(raw).map_or(raw, |s| s.as_str());
        if self.drop_fields.contains(resolved) {
            return None;
        }
        Some(resolved)
    }
}

/// Comparison operator for column-to-column filters (evaluated post-reduce).
#[derive(Clone, Debug)]
pub enum CompareOp {
    Gt,
    Lt,
    Ge,
    Le,
    Eq,
    Ne,
}

impl CompareOp {
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            ">" | "gt" => Some(CompareOp::Gt),
            "<" | "lt" => Some(CompareOp::Lt),
            ">=" | "ge" => Some(CompareOp::Ge),
            "<=" | "le" => Some(CompareOp::Le),
            "==" | "eq" => Some(CompareOp::Eq),
            "!=" | "ne" => Some(CompareOp::Ne),
            _ => None,
        }
    }

    /// Map to pyarrow.compute function name.
    pub fn compute_fn(&self) -> &'static str {
        match self {
            CompareOp::Gt => "greater",
            CompareOp::Lt => "less",
            CompareOp::Ge => "greater_equal",
            CompareOp::Le => "less_equal",
            CompareOp::Eq => "equal",
            CompareOp::Ne => "not_equal",
        }
    }
}

/// A filter predicate evaluated per-row during parsing.
#[derive(Clone, Debug)]
pub enum FilterPredicate {
    /// Keep row if `field_value != value` (string comparison, per-row).
    NotEqual { field: String, value: String },
    /// Keep row if `field_value == value` (string comparison, per-row).
    Equal { field: String, value: String },
    /// Column-to-column comparison evaluated post-reduce via pyarrow.compute.
    Compare { field_a: String, op: CompareOp, field_b: String },
}

impl FilterPredicate {
    /// Check whether a partial row passes the filter.
    /// `columns` contains all builders; `row_index` is the current
    /// row number (before finishing).  Returns true to keep the row.
    /// The filter field is resolved through `plan.field_map` before lookup.
    /// Compare filters always return true (evaluated post-reduce).
    pub(crate) fn check(
        &self,
        columns: &HashMap<String, ColumnBuilder>,
        row_index: usize,
        plan: &BuildPlan,
    ) -> bool {
        let (field, expected) = match self {
            FilterPredicate::NotEqual { field, value } => (field, value),
            FilterPredicate::Equal { field, value } => (field, value),
            FilterPredicate::Compare { .. } => return true,
        };
        // Resolve the filter field name: if renamed, use the new name.
        let resolved = plan.field_map.get(field).map_or(field, |s| s);

        let actual = columns
            .get(resolved)
            .and_then(|b| b.get_filter_value(row_index));
        let actual = actual.as_deref();
        match self {
            FilterPredicate::NotEqual { .. } => actual != Some(expected),
            FilterPredicate::Equal { .. } => actual == Some(expected),
            FilterPredicate::Compare { .. } => true,
        }
    }

    /// Apply a Compare filter over the pyarrow table, returning a filtered
    /// table.  For per-row filters this is a no-op (returns the table as-is).
    pub(crate) fn apply_pyarrow(
        &self,
        table: PyObject,
        py: Python<'_>,
    ) -> PyResult<PyObject> {
        match self {
            FilterPredicate::Compare { field_a, op, field_b } => {
                let pc = PyModule::import(py, "pyarrow.compute")?;
                let fn_name = op.compute_fn();
                let col_a = table.getattr(py, "column")?.call1(py, (field_a,))?;
                let col_b = table.getattr(py, "column")?.call1(py, (field_b,))?;
                let mask = pc.call_method1(fn_name, (col_a, col_b))?;
                table.getattr(py, "filter")?.call1(py, (mask,))
            }
            _ => Ok(table),
        }
    }
}

/// Flat string column storage in arrow layout: one contiguous byte arena +
/// offsets + validity. No per-cell String allocation, and arrow export is a
/// block copy of two buffers instead of a per-value re-copy.
#[derive(Default)]
pub(crate) struct StrColumn {
    data: Vec<u8>,
    /// len + 1 entries; offsets[i]..offsets[i+1] is value i.
    offsets: Vec<i32>,
    validity: Vec<bool>,
}

impl StrColumn {
    fn with_capacity(cap: usize) -> Self {
        let mut offsets = Vec::with_capacity(cap + 1);
        offsets.push(0);
        StrColumn {
            data: Vec::with_capacity(cap * 16),
            offsets,
            validity: Vec::with_capacity(cap),
        }
    }

    fn push(&mut self, v: Option<&str>) {
        if let Some(s) = v {
            self.data.extend_from_slice(s.as_bytes());
        }
        self.offsets.push(self.data.len() as i32);
        self.validity.push(v.is_some());
    }

    fn pop(&mut self) {
        if self.validity.pop().is_some() {
            self.offsets.pop();
            self.data.truncate(*self.offsets.last().unwrap() as usize);
        }
    }

    fn len(&self) -> usize {
        self.validity.len()
    }

    fn get(&self, i: usize) -> Option<&str> {
        if !*self.validity.get(i)? {
            return None;
        }
        let start = self.offsets[i] as usize;
        let end = self.offsets[i + 1] as usize;
        std::str::from_utf8(&self.data[start..end]).ok()
    }

    /// Move all values from `other` onto the end of `self`.
    fn append(&mut self, other: &StrColumn) {
        let base = self.data.len() as i32;
        self.data.extend_from_slice(&other.data);
        self.offsets
            .extend(other.offsets[1..].iter().map(|o| o + base));
        self.validity.extend_from_slice(&other.validity);
    }

    fn iter(&self) -> impl Iterator<Item = Option<&str>> {
        (0..self.len()).map(move |i| self.get(i))
    }

    fn to_arrow(&self) -> Result<ArrayRef, String> {
        use arrow::buffer::{Buffer, NullBuffer, OffsetBuffer, ScalarBuffer};
        let offsets = OffsetBuffer::new(ScalarBuffer::from(self.offsets.clone()));
        let data = Buffer::from_slice_ref(&self.data);
        let nulls = if self.validity.iter().all(|&v| v) {
            None
        } else {
            Some(NullBuffer::from(self.validity.clone()))
        };
        let arr = StringArray::try_new(offsets, data, nulls).map_err(|e| e.to_string())?;
        Ok(Arc::new(arr))
    }
}

/// Per-column builder: stores all values.  The variant determines
/// the storage type (String, Int64, Float64, Boolean, or Dictionary).
pub(crate) enum ColumnBuilder {
    String(StrColumn),
    Int64(Vec<Option<i64>>),
    Float64(Vec<Option<f64>>),
    Boolean(Vec<Option<bool>>),
    Dictionary {
        codes: Vec<Option<i32>>,
        dict: Vec<String>,
        /// value → code side-index; linear scans over `dict` were O(n) per
        /// pushed value (quadratic overall).
        index: HashMap<String, i32>,
    },
}

/// Look up `v` in the dictionary index, inserting a new code if absent.
fn dict_code(dict: &mut Vec<String>, index: &mut HashMap<String, i32>, v: &str) -> i32 {
    if let Some(&code) = index.get(v) {
        return code;
    }
    let code = dict.len() as i32;
    dict.push(v.to_owned());
    index.insert(v.to_owned(), code);
    code
}

impl ColumnBuilder {
    fn with_capacity(cap: usize, field_type: &FieldType) -> Self {
        match field_type {
            FieldType::String => ColumnBuilder::String(StrColumn::with_capacity(cap)),
            FieldType::Int64 => ColumnBuilder::Int64(Vec::with_capacity(cap)),
            FieldType::Float64 => ColumnBuilder::Float64(Vec::with_capacity(cap)),
            FieldType::Boolean => ColumnBuilder::Boolean(Vec::with_capacity(cap)),
            FieldType::Dictionary => ColumnBuilder::Dictionary {
                codes: Vec::with_capacity(cap),
                dict: Vec::new(),
                index: HashMap::default(),
            },
        }
    }

    /// Push a value.  For typed builders (Int64/Float64/Boolean),
    /// unparseable values become `None` (null), not an error.
    /// This applies per-chunk: cross-chunk type conflicts resolve
    /// to null as well (widest type is the declared type).
    fn push(&mut self, value: Option<String>) {
        match self {
            ColumnBuilder::String(v) => v.push(value.as_deref()),
            ColumnBuilder::Int64(v) => {
                v.push(value.and_then(|s| lexical::parse::<i64, _>(s.as_bytes()).ok()));
            }
            ColumnBuilder::Float64(v) => {
                v.push(value.and_then(|s| lexical::parse::<f64, _>(s.as_bytes()).ok()));
            }
            ColumnBuilder::Boolean(v) => {
                v.push(value.and_then(|s| s.parse::<bool>().ok()));
            }
            ColumnBuilder::Dictionary { codes, dict, index } => match value {
                Some(v) => {
                    let idx = dict_code(dict, index, &v);
                    codes.push(Some(idx));
                }
                None => codes.push(None),
            },
        }
    }

    /// Push a borrowed str — avoids `into_owned()` allocation for
    /// typed columns that parse and discard the string.
    fn push_str(&mut self, value: Option<&str>) {
        match self {
            ColumnBuilder::String(v) => v.push(value),
            ColumnBuilder::Int64(v) => {
                v.push(value.and_then(|s| lexical::parse::<i64, _>(s.as_bytes()).ok()));
            }
            ColumnBuilder::Float64(v) => {
                v.push(value.and_then(|s| lexical::parse::<f64, _>(s.as_bytes()).ok()));
            }
            ColumnBuilder::Boolean(v) => {
                v.push(value.and_then(|s| s.parse::<bool>().ok()));
            }
            ColumnBuilder::Dictionary { codes, dict, index } => match value {
                Some(v) => {
                    let idx = dict_code(dict, index, v);
                    codes.push(Some(idx));
                }
                None => codes.push(None),
            },
        }
    }

    fn pop(&mut self) {
        match self {
            ColumnBuilder::String(v) => v.pop(),
            ColumnBuilder::Int64(v) => drop(v.pop()),
            ColumnBuilder::Float64(v) => drop(v.pop()),
            ColumnBuilder::Boolean(v) => drop(v.pop()),
            ColumnBuilder::Dictionary { codes, .. } => drop(codes.pop()),
        }
    }

    fn len(&self) -> usize {
        match self {
            ColumnBuilder::String(v) => v.len(),
            ColumnBuilder::Int64(v) => v.len(),
            ColumnBuilder::Float64(v) => v.len(),
            ColumnBuilder::Boolean(v) => v.len(),
            ColumnBuilder::Dictionary { codes, .. } => codes.len(),
        }
    }

    /// Value at `index` formatted as a string for filter comparison.
    fn get_filter_value(&self, index: usize) -> Option<String> {
        match self {
            ColumnBuilder::String(v) => v.get(index).map(|s| s.to_owned()),
            ColumnBuilder::Int64(v) => v.get(index).map(|o| o.map(|n| n.to_string())).unwrap_or(None),
            ColumnBuilder::Float64(v) => v.get(index).map(|o| o.map(|n| n.to_string())).unwrap_or(None),
            ColumnBuilder::Boolean(v) => v.get(index).map(|o| o.map(|n| n.to_string())).unwrap_or(None),
            ColumnBuilder::Dictionary { codes, dict, .. } => codes
                .get(index)
                .and_then(|code| code.map(|idx| dict[idx as usize].clone())),
        }
    }

    /// Merge all values from `other` into `self`, consuming `other` — values
    /// are moved (Vec::append), never cloned. Both must be the same variant.
    /// Returns `Err` if the two builders are different variants.
    fn extend_owned(&mut self, other: ColumnBuilder) -> Result<(), String> {
        match (self, other) {
            (ColumnBuilder::String(a), ColumnBuilder::String(b)) => {
                a.append(&b);
            }
            (ColumnBuilder::Int64(a), ColumnBuilder::Int64(mut b)) => {
                a.append(&mut b);
            }
            (ColumnBuilder::Float64(a), ColumnBuilder::Float64(mut b)) => {
                a.append(&mut b);
            }
            (ColumnBuilder::Boolean(a), ColumnBuilder::Boolean(mut b)) => {
                a.append(&mut b);
            }
            (ColumnBuilder::Dictionary { codes: a_codes, dict: a_dict, index: a_index },
             ColumnBuilder::Dictionary { codes: b_codes, dict: b_dict, .. }) => {
                // Remap b's dictionary into a's once, then translate codes.
                let remap: Vec<i32> = b_dict
                    .iter()
                    .map(|val| dict_code(a_dict, a_index, val))
                    .collect();
                a_codes.extend(
                    b_codes.iter().map(|c| c.map(|idx| remap[idx as usize])),
                );
            }
            _ => return Err("extend_owned: column type mismatch across chunks".to_string()),
        }
        Ok(())
    }

    /// Upgrade a String builder to Dictionary if cardinality is low enough.
    /// No-op if not String, or if rows < min_rows.
    fn try_upgrade_to_dict(&mut self, min_rows: usize) {
        let old = match self {
            ColumnBuilder::String(v) => std::mem::take(v),
            _ => return,
        };
        if old.len() < min_rows {
            *self = ColumnBuilder::String(old);
            return;
        }
        // Count distinct values
        let mut seen: HashSet<&str> = HashSet::default();
        for v in old.iter() {
            if let Some(s) = v {
                seen.insert(s);
            }
        }
        // Threshold: at most 5% distinct, clamped to [16, 256]
        let threshold = (old.len() / 20).max(16).min(256);
        if seen.len() > threshold {
            *self = ColumnBuilder::String(old);
            return;
        }
        // Upgrade: build dictionary + codes
        let mut dict: Vec<String> = Vec::new();
        let mut index: HashMap<String, i32> = HashMap::default();
        let mut codes: Vec<Option<i32>> = Vec::with_capacity(old.len());
        for v in old.iter() {
            match v {
                Some(s) => {
                    let idx = dict_code(&mut dict, &mut index, s);
                    codes.push(Some(idx));
                }
                None => codes.push(None),
            }
        }
        *self = ColumnBuilder::Dictionary { codes, dict, index };
    }

    /// Arrow logical type for this column (used to build the schema Field).
    fn arrow_datatype(&self) -> DataType {
        match self {
            ColumnBuilder::String(_) => DataType::Utf8,
            ColumnBuilder::Int64(_) => DataType::Int64,
            ColumnBuilder::Float64(_) => DataType::Float64,
            ColumnBuilder::Boolean(_) => DataType::Boolean,
            ColumnBuilder::Dictionary { .. } => {
                DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8))
            }
        }
    }

    /// Build a native Arrow array from the column builder.
    /// No per-cell Python objects are created.
    fn to_arrow_array(&self) -> Result<ArrayRef, String> {
        Ok(match self {
            ColumnBuilder::String(v) => v.to_arrow()?,
            ColumnBuilder::Int64(v) => {
                Arc::new(v.iter().copied().collect::<Int64Array>())
            }
            ColumnBuilder::Float64(v) => {
                Arc::new(v.iter().copied().collect::<Float64Array>())
            }
            ColumnBuilder::Boolean(v) => {
                Arc::new(v.iter().copied().collect::<BooleanArray>())
            }
            ColumnBuilder::Dictionary { codes, dict, .. } => {
                let keys: Int32Array = codes.iter().copied().collect();
                let values: ArrayRef = Arc::new(
                    dict.iter().map(|s| Some(s.as_str())).collect::<StringArray>(),
                );
                let arr = DictionaryArray::<Int32Type>::try_new(keys, values)
                    .map_err(|e| e.to_string())?;
                Arc::new(arr)
            }
        })
    }

    #[cfg(test)]
    fn as_str_vec(&self) -> Vec<Option<String>> {
        match self {
            ColumnBuilder::String(v) => v.iter().map(|o| o.map(str::to_owned)).collect(),
            _ => panic!("as_str_vec called on non-String ColumnBuilder"),
        }
    }
}

/// Bytes are chunk-validated UTF-8 (see `parse_bytes` entry) — skip
/// std's per-call revalidation.
#[allow(unsafe_code)]
#[inline]
fn utf8_unchecked(b: &[u8]) -> &str {
    unsafe { std::str::from_utf8_unchecked(b) }
}

/// Attribute value without revalidation; unescapes only when an entity is
/// actually present (memchr probe — CR values almost never contain `&`).
fn attr_value<'v>(
    attr: &quick_xml::events::attributes::Attribute<'v>,
) -> Result<std::borrow::Cow<'v, str>, String> {
    use std::borrow::Cow;
    match &attr.value {
        Cow::Borrowed(b) => {
            let s = utf8_unchecked(b);
            if memchr::memchr(b'&', b).is_none() {
                Ok(Cow::Borrowed(s))
            } else {
                quick_xml::escape::unescape(s).map_err(|e| e.to_string())
            }
        }
        // Owned never occurs for the borrowed-slice reader; fall back.
        Cow::Owned(_) => attr
            .unescape_value()
            .map_err(|e| e.to_string())
            .map(|c| Cow::Owned(c.into_owned())),
    }
}

/// Text content without revalidation; same `&` probe as `attr_value`.
fn text_value(txt: quick_xml::events::BytesText<'_>) -> Result<std::borrow::Cow<'_, str>, String> {
    use std::borrow::Cow;
    match txt.into_inner() {
        Cow::Borrowed(b) => {
            let s = utf8_unchecked(b);
            if memchr::memchr(b'&', b).is_none() {
                Ok(Cow::Borrowed(s))
            } else {
                quick_xml::escape::unescape(s).map_err(|e| e.to_string())
            }
        }
        // Owned never occurs for the borrowed-slice reader; fall back.
        Cow::Owned(o) => {
            let s = String::from_utf8(o).map_err(|e| e.to_string())?;
            Ok(Cow::Owned(
                match quick_xml::escape::unescape(&s).map_err(|e| e.to_string())? {
                    Cow::Borrowed(x) => x.to_owned(),
                    Cow::Owned(x) => x,
                },
            ))
        }
    }
}

/// Raw byte value known to be chunk-validated UTF-8; unescape only when an
/// entity is present.

/// Columnar engine: parses XML rows into column-oriented storage,
/// then exports to a PyArrow table in one shot.
pub struct ColumnarEngine {
    columns: HashMap<String, ColumnBuilder>,
    column_order: Vec<String>,
    row_count: usize,
    estimated_rows: usize,
    plan: BuildPlan,
    #[cfg(feature = "profile")]
    profile: ColumnarProfileCounters,
}

#[cfg(feature = "profile")]
#[derive(Default, Clone)]
pub struct ColumnarProfileCounters {
    pub parse_ns: u64,
    pub export_ns: u64,
    pub event_loop_ns: u64,
    pub unescape_ns: u64,
    pub copy_ns: u64,
}

impl ColumnarEngine {
    fn trailing_close_tags_only(bytes: &[u8], mut pos: usize) -> bool {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }

        if pos >= bytes.len() {
            return true;
        }

        while pos < bytes.len() {
            if bytes[pos] != b'<' || pos + 1 >= bytes.len() || bytes[pos + 1] != b'/' {
                return false;
            }
            pos += 2;

            while pos < bytes.len() && bytes[pos] != b'>' {
                pos += 1;
            }
            if pos >= bytes.len() {
                return false;
            }
            pos += 1;

            while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                pos += 1;
            }
        }

        true
    }

    pub fn new() -> Self {
        ColumnarEngine {
            columns: HashMap::default(),
            column_order: Vec::new(),
            row_count: 0,
            estimated_rows: 0,
            plan: BuildPlan::new(),
            #[cfg(feature = "profile")]
            profile: ColumnarProfileCounters::default(),
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        ColumnarEngine {
            columns: HashMap::default(),
            column_order: Vec::new(),
            row_count: 0,
            estimated_rows: cap,
            plan: BuildPlan::new(),
            #[cfg(feature = "profile")]
            profile: ColumnarProfileCounters::default(),
        }
    }

    pub fn with_plan(cap: usize, plan: BuildPlan) -> Self {
        ColumnarEngine {
            columns: HashMap::default(),
            column_order: Vec::new(),
            row_count: 0,
            estimated_rows: cap,
            plan,
            #[cfg(feature = "profile")]
            profile: ColumnarProfileCounters::default(),
        }
    }

    fn schema_insert_index(&self, name: &str) -> usize {
        let order = &self.plan.schema_order;
        if order.is_empty() {
            return self.column_order.len();
        }
        let pos = order.iter().position(|n| n == name);
        match pos {
            Some(p) => self
                .column_order
                .iter()
                .position(|existing| {
                    order.iter().position(|n| n == existing).map_or(false, |ep| ep > p)
                })
                .unwrap_or(self.column_order.len()),
            None => self.column_order.len(),
        }
    }

    fn ensure_column(&mut self, name: &str) {
        if !self.columns.contains_key(name) {
            let est = self.estimated_rows.max(64);
            let col_type = self.plan.column_type(name);
            let mut b = ColumnBuilder::with_capacity(est, &col_type);
            for _ in 0..self.row_count {
                b.push(None);
            }
            self.columns.insert(name.to_owned(), b);
            let idx = self.schema_insert_index(name);
            self.column_order.insert(idx, name.to_owned());
        }
    }

    fn push_field(&mut self, name: &str, value: Option<String>) {
        // Resolve field: rename or drop.  Copy to owned String to
        // break the borrow on self.plan before the mutable borrow.
        let resolved = match self.plan.resolve_field(name) {
            Some(n) => n.to_owned(),
            None => return,
        };
        self.ensure_column(&resolved);
        if let Some(b) = self.columns.get_mut(&resolved) {
            // Last-write-wins: if this column was already pushed for the
            // current (incomplete) row, overwrite instead of append.
            if b.len() > self.row_count {
                b.pop();
            }
            b.push(value);
        }
    }

    /// Push a borrowed str — the builder owns the value only when its
    /// storage requires it (String copies; Dictionary allocs only for new
    /// dictionary entries; typed columns parse and never allocate).
    fn push_field_str(&mut self, name: &str, value: Option<&str>) {
        // Fast path: no rename/drop configured — skip the plan lookup and
        // the owned copy it needs to break the borrow on self.plan.
        let owned;
        let resolved: &str = if self.plan.field_map.is_empty() && self.plan.drop_fields.is_empty() {
            name
        } else {
            match self.plan.resolve_field(name) {
                Some(n) => {
                    owned = n.to_owned();
                    &owned
                }
                None => return,
            }
        };
        self.ensure_column(resolved);
        let row_count = self.row_count;
        if let Some(b) = self.columns.get_mut(resolved) {
            if b.len() > row_count {
                b.pop();
            }
            b.push_str(value);
        }
    }

    /// Null-fill any column missing this row, then apply filter.
    /// If the filter rejects the row, undo it by popping values.
    fn finish_row(&mut self) {
        let target = self.row_count + 1;
        for b in self.columns.values_mut() {
            while b.len() < target {
                b.push(None);
            }
        }

        // Check filter; if the row fails, undo the append.
        if let Some(ref filter) = self.plan.filter {
            if !filter.check(&self.columns, self.row_count, &self.plan) {
                for b in self.columns.values_mut() {
                    b.pop();
                }
                return;
            }
        }

        self.row_count += 1;
    }

    /// Parse a complete byte slice into columnar storage.
    ///
    /// `row_tag` is the XML element name for data rows (e.g. `b"Row"`).
    ///
    /// Uses a streaming quick-xml reader.  When the reader hits an error
    /// (unmatched parent end tag), falls back to per-row readers for the
    /// remaining data in the chunk.
    /// Fast path: hand-rolled flat scanner for the fixed CR row shape.
    ///
    /// Mirrors `parse_bytes_quickxml` exactly for the shapes it accepts:
    /// row attributes, flat Field/Text/Section/unknown children, value text
    /// from FormattedValue/Value/TextValue, stray end tags ignored, EOF
    /// mid-row leaves the partial row for `normalize()` to pop. Anything

    #[cfg(feature = "testing")]
    pub fn parse_bytes_quickxml_only(&mut self, bytes: &[u8], row_tag: &[u8]) -> Result<(), String> {
        simdutf8::basic::from_utf8(bytes)
            .map_err(|_| "invalid UTF-8 in input".to_string())?;
        self.parse_bytes_quickxml(bytes, row_tag)
    }

    pub fn parse_bytes(&mut self, bytes: &[u8], row_tag: &[u8]) -> Result<(), String> {
        simdutf8::basic::from_utf8(bytes)
            .map_err(|_| "invalid UTF-8 in input".to_string())?;
        #[cfg(feature = "profile")]
        let _start = std::time::Instant::now();
        self.parse_bytes_quickxml(bytes, row_tag)?;
        #[cfg(feature = "profile")]
        {
            self.profile.parse_ns += _start.elapsed().as_nanos() as u64;
        }
        Ok(())
    }

    /// Undo everything pushed since `rows` committed rows (partial row
    fn parse_bytes_quickxml(&mut self, bytes: &[u8], row_tag: &[u8]) -> Result<(), String> {
        // Borrowed-slice reader: events reference `bytes` directly, so no
        // event is ever copied into a scratch buffer (the Cursor +
        // read_event_into variant showed up as memmove in profiles).
        let mut reader = Reader::from_reader(bytes);
        reader.config_mut().check_end_names = false;

        let row_tag_owned = row_tag.to_vec();

        loop {
            #[cfg(feature = "profile")]
            let _ev_start = std::time::Instant::now();
            let event = match reader.read_event() {
                Ok(e) => e,
                Err(e) => {
                    let err_msg = e.to_string();
                    if err_msg.contains("close tag") {
                        let err_pos = reader.buffer_position() as usize;
                        self.parse_tail(bytes, row_tag, err_pos)?;
                        return Ok(());
                    }
                    let err_pos = reader.buffer_position() as usize;
                    if Self::trailing_close_tags_only(bytes, err_pos) {
                        return Ok(());
                    }
                    return Err(format!(
                        "malformed XML at byte {}: {}",
                        reader.buffer_position(),
                        e
                    ));
                }
            };
            #[cfg(feature = "profile")]
            {
                self.profile.event_loop_ns += _ev_start.elapsed().as_nanos() as u64;
            }

            match event {
                Event::Empty(ref e) if e.name().as_ref() == row_tag_owned => {
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| e.to_string())?;
                        let key = utf8_unchecked(attr.key.as_ref());
                        #[cfg(feature = "profile")]
                        let _un_start = std::time::Instant::now();
                        let value = attr_value(&attr)?;
                        #[cfg(feature = "profile")]
                        let _ = self.profile.unescape_ns += _un_start.elapsed().as_nanos() as u64;
                        #[cfg(feature = "profile")]
                        let _cp_start = std::time::Instant::now();
                        self.push_field_str(key, Some(value.as_ref()));
                        #[cfg(feature = "profile")]
                        let _ = self.profile.copy_ns += _cp_start.elapsed().as_nanos() as u64;
                    }
                    self.finish_row();
                }

                Event::Start(ref e) if e.name().as_ref() == row_tag_owned => {
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| e.to_string())?;
                        let key = utf8_unchecked(attr.key.as_ref());
                        #[cfg(feature = "profile")]
                        let _un_start = std::time::Instant::now();
                        let value = attr_value(&attr)?;
                        #[cfg(feature = "profile")]
                        let _ = self.profile.unescape_ns += _un_start.elapsed().as_nanos() as u64;
                        #[cfg(feature = "profile")]
                        let _cp_start = std::time::Instant::now();
                        self.push_field_str(key, Some(value.as_ref()));
                        #[cfg(feature = "profile")]
                        let _ = self.profile.copy_ns += _cp_start.elapsed().as_nanos() as u64;
                    }

                    loop {
                        let child_event = reader
                            .read_event()
                            .map_err(|e| e.to_string())?;

                        match child_event {
                            Event::Start(ref child) | Event::Empty(ref child) => {
                                let child_name = child.name();
                                let child_tag = child_name.as_ref();

                                if child_tag == b"Field" {
                                    let mut field_name = None;
                                    for attr in child.attributes() {
                                        if let Ok(attr) = attr {
                                            let attr_key = attr.key.as_ref();
                                            if attr_key == b"FieldName"
                                                || attr_key == b"Name"
                                            {
                                                #[cfg(feature = "profile")]
                                                let _un_start = std::time::Instant::now();
                                                let value = attr_value(&attr);
                                                #[cfg(feature = "profile")]
                                                let _ = self.profile.unescape_ns += _un_start.elapsed().as_nanos() as u64;
                                                if let Ok(value) = value {
                                                    field_name = Some(value);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    let key: &str =
                                        field_name.as_deref().unwrap_or("Field");

                                    let mut text = std::borrow::Cow::Borrowed("");
                                    if matches!(child_event, Event::Start(_)) {
                                        let field_end_bytes: &[u8] = b"Field";
                                        loop {
                                            let inner = reader
                                                .read_event()
                                                .map_err(|e| e.to_string())?;
                                            match inner {
                                                Event::Start(ref inner_child)
                                                | Event::Empty(ref inner_child) => {
                                                    let inner_name = inner_child.name();
                                                    let inner_tag = inner_name.as_ref();
                                                    if inner_tag == b"FormattedValue"
                                                        || inner_tag == b"Value"
                                                    {
                                                        if matches!(inner, Event::Start(_)) {
                                                            let text_event = reader
                                                                .read_event()
                                                                .map_err(|e| {
                                                                    e.to_string()
                                                                })?;
                                                            if let Event::Text(txt) =
                                                                text_event
                                                            {
                                                                #[cfg(feature = "profile")]
                                                                let _un_start = std::time::Instant::now();
                                                                text = text_value(txt)?;
                                                                #[cfg(feature = "profile")]
                                                                let _ = self.profile.unescape_ns += _un_start.elapsed().as_nanos() as u64;
                                                            }
                                                        }
                                                    }
                                                }
                                                Event::End(ref e)
                                                    if e.name().as_ref() == field_end_bytes =>
                                                {
                                                    break;
                                                }
                                                Event::Eof => return Ok(()),
                                                _ => {}
                                            }
                                        }
                                    }
                                    #[cfg(feature = "profile")]
                                    let _cp_start = std::time::Instant::now();
                                    self.push_field_str(key, Some(text.as_ref()));
                                    #[cfg(feature = "profile")]
                                    let _ = self.profile.copy_ns += _cp_start.elapsed().as_nanos() as u64;
                                } else if child_tag == b"Text" {
                                    let mut text_name = None;
                                    for attr in child.attributes() {
                                        if let Ok(attr) = attr {
                                            if attr.key.as_ref() == b"Name" {
                                                #[cfg(feature = "profile")]
                                                let _un_start = std::time::Instant::now();
                                                let value = attr_value(&attr);
                                                #[cfg(feature = "profile")]
                                                let _ = self.profile.unescape_ns += _un_start.elapsed().as_nanos() as u64;
                                                if let Ok(value) = value {
                                                    text_name = Some(value);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    let key: &str =
                                        text_name.as_deref().unwrap_or("Text");

                                    let mut text = std::borrow::Cow::Borrowed("");
                                    if matches!(child_event, Event::Start(_)) {
                                        let text_end_bytes: &[u8] = b"Text";
                                        loop {
                                            let inner = reader
                                                .read_event()
                                                .map_err(|e| e.to_string())?;
                                            match inner {
                                                Event::Start(ref inner_child)
                                                | Event::Empty(ref inner_child) => {
                                                    let ic_name = inner_child.name();
                                                    if ic_name.as_ref()
                                                        == b"TextValue"
                                                    {
                                                        if matches!(inner, Event::Start(_)) {
                                                            let text_event = reader
                                                                .read_event()
                                                                .map_err(|e| {
                                                                    e.to_string()
                                                                })?;
                                                            if let Event::Text(txt) =
                                                                text_event
                                                            {
                                                                #[cfg(feature = "profile")]
                                                                let _un_start = std::time::Instant::now();
                                                                text = text_value(txt)?;
                                                                #[cfg(feature = "profile")]
                                                                let _ = self.profile.unescape_ns += _un_start.elapsed().as_nanos() as u64;
                                                            }
                                                        }
                                                    }
                                                }
                                                Event::End(ref e)
                                                    if e.name().as_ref()
                                                        == text_end_bytes =>
                                                {
                                                    break;
                                                }
                                                Event::Eof => return Ok(()),
                                                _ => {}
                                            }
                                        }
                                    }
                                    #[cfg(feature = "profile")]
                                    let _cp_start = std::time::Instant::now();
                                    self.push_field_str(key, Some(text.as_ref()));
                                    #[cfg(feature = "profile")]
                                    let _ = self.profile.copy_ns += _cp_start.elapsed().as_nanos() as u64;
                                } else if child_tag == b"Section" {
                                    let sn = child
                                        .attributes()
                                        .filter_map(|a| a.ok())
                                        .find(|a| a.key.as_ref() == b"SectionNumber")
                                        .and_then(|a| {
                                            #[cfg(feature = "profile")]
                                            let _un_start = std::time::Instant::now();
                                            let result = attr_value(&a).ok();
                                            #[cfg(feature = "profile")]
                                            {
                                                self.profile.unescape_ns += _un_start.elapsed().as_nanos() as u64;
                                            }
                                            result
                                        })
                                        .unwrap_or_default();
                                    #[cfg(feature = "profile")]
                                    let _cp_start = std::time::Instant::now();
                                    self.push_field_str("Section", Some(sn.as_ref()));
                                    #[cfg(feature = "profile")]
                                    let _ = self.profile.copy_ns += _cp_start.elapsed().as_nanos() as u64;
                                } else {
                                    let key = utf8_unchecked(child_tag);
                                    #[cfg(feature = "profile")]
                                    let _cp_start = std::time::Instant::now();
                                    self.push_field_str(key, Some(""));
                                    #[cfg(feature = "profile")]
                                    let _ = self.profile.copy_ns += _cp_start.elapsed().as_nanos() as u64;
                                }
                            }

                            Event::End(ref e)
                                if e.name().as_ref() == row_tag_owned =>
                            {
                                break;
                            }
                            Event::Eof => return Ok(()),
                            _ => {}
                        }
                    }

                    self.finish_row();
                }

                Event::Eof => return Ok(()),
                _ => {}
            }
        }
    }

    /// Fallback recovery for chunked parsing: resume by scanning for the next
    /// row start and parsing rows individually. This handles orphan parent
    /// close-tags that are valid in the full document but not within a chunk.
    fn parse_tail(&mut self, bytes: &[u8], row_tag: &[u8], start_pos: usize) -> Result<(), String> {
        let (skip_regions, _) = crate::splitter::find_special_regions(bytes);
        let mut pos: usize = start_pos;
        let row_tag_owned = row_tag.to_vec();

        while let Some(row_start) =
            crate::splitter::next_row_start(bytes, pos, row_tag, &skip_regions)
        {
            let row_bytes = &bytes[row_start..];
            let mut rr = Reader::from_reader(Cursor::new(row_bytes));
            rr.config_mut().check_end_names = false;
            let mut buf = Vec::with_capacity(4096);

            let ev = match rr.read_event_into(&mut buf) {
                Ok(e) => e,
                Err(_) => break,
            };

            match ev {
                Event::Empty(ref e) if e.name().as_ref() == row_tag_owned => {
                    for attr in e.attributes() {
                        if let Ok(a) = attr {
                            let key =
                                std::str::from_utf8(a.key.as_ref()).unwrap_or("").to_owned();
                            let value = a.unescape_value().unwrap_or_default().into_owned();
                            self.push_field(&key, Some(value));
                        }
                    }
                    self.finish_row();
                }
                Event::Start(ref e) if e.name().as_ref() == row_tag_owned => {
                    for attr in e.attributes() {
                        if let Ok(a) = attr {
                            let key =
                                std::str::from_utf8(a.key.as_ref()).unwrap_or("").to_owned();
                            let value = a.unescape_value().unwrap_or_default().into_owned();
                            self.push_field(&key, Some(value));
                        }
                    }

                    loop {
                        let child = match rr.read_event_into(&mut buf) {
                            Ok(e) => e,
                            Err(_) => break,
                        };
                        match child {
                            Event::Start(ref c) | Event::Empty(ref c) => {
                                let tag = c.name().as_ref().to_vec();
                                if tag == b"Field" {
                                    let mut name = String::from("Field");
                                    for attr in c.attributes() {
                                        if let Ok(a) = attr {
                                            let k = a.key.as_ref();
                                            if k == b"FieldName" || k == b"Name" {
                                                if let Ok(v) = a.unescape_value() {
                                                    name = v.into_owned();
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    let mut text = String::new();
                                    if matches!(child, Event::Start(_)) {
                                        let end = tag;
                                        loop {
                                            let inner = rr.read_event_into(&mut buf);
                                            match inner {
                                                Ok(Event::Start(ic)) => {
                                                    let ic_name = ic.name().as_ref().to_vec();
                                                    if ic_name == b"FormattedValue"
                                                        || ic_name == b"Value"
                                                    {
                                                        if let Ok(Event::Text(txt)) =
                                                            rr.read_event_into(&mut buf)
                                                        {
                                                            if let Ok(v) = txt.unescape() {
                                                                text = v.into_owned();
                                                            }
                                                        }
                                                    }
                                                }
                                                Ok(Event::Empty(ic)) => {
                                                    let ic_name = ic.name().as_ref().to_vec();
                                                    if ic_name == b"FormattedValue"
                                                        || ic_name == b"Value"
                                                    {
                                                        if let Ok(Event::Text(txt)) =
                                                            rr.read_event_into(&mut buf)
                                                        {
                                                            if let Ok(v) = txt.unescape() {
                                                                text = v.into_owned();
                                                            }
                                                        }
                                                    }
                                                }
                                                Ok(Event::End(ref ne)) if ne.name().as_ref() == end => {
                                                    break;
                                                }
                                                Ok(Event::Eof) => return Ok(()),
                                                _ => {}
                                            }
                                        }
                                    }
                                    self.push_field(&name, Some(text));
                                } else if tag == b"Text" {
                                    let mut name = String::from("Text");
                                    for attr in c.attributes() {
                                        if let Ok(a) = attr {
                                            if a.key.as_ref() == b"Name" {
                                                if let Ok(v) = a.unescape_value() {
                                                    name = v.into_owned();
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    if matches!(child, Event::Start(_)) {
                                        let end = tag;
                                        let mut text = String::new();
                                        loop {
                                            match rr.read_event_into(&mut buf) {
                                                Ok(Event::Start(ref ic))
                                                | Ok(Event::Empty(ref ic)) => {
                                                    if ic.name().as_ref() == b"TextValue" {
                                                        if let Ok(Event::Text(txt)) =
                                                            rr.read_event_into(&mut buf)
                                                        {
                                                            if let Ok(v) = txt.unescape() {
                                                                text = v.into_owned();
                                                            }
                                                        }
                                                    }
                                                }
                                                Ok(Event::End(ref ne))
                                                    if ne.name().as_ref() == end =>
                                                {
                                                    break;
                                                }
                                                Ok(Event::Eof) => return Ok(()),
                                                _ => {}
                                            }
                                        }
                                        if !text.is_empty() {
                                            self.push_field(&name, Some(text));
                                        }
                                    }
                                } else if tag == b"Section" {
                                    let sn = c
                                        .attributes()
                                        .filter_map(|a| a.ok())
                                        .find(|a| a.key.as_ref() == b"SectionNumber")
                                        .and_then(|a| a.unescape_value().ok())
                                        .unwrap_or_default()
                                        .into_owned();
                                    self.push_field("Section", Some(sn));
                                } else {
                                    let key = std::str::from_utf8(&tag).unwrap_or("").to_owned();
                                    self.push_field(&key, Some(String::new()));
                                }
                            }
                            Event::End(ref e) if e.name().as_ref() == row_tag_owned => break,
                            Event::Eof => return Ok(()),
                            _ => {}
                        }
                    }
                    self.finish_row();
                }
                _ => {}
            }
            pos = row_start + 1;
        }
        Ok(())
    }

    /// Merge another engine's data into this one (multi-chunk reduce).
    ///
    /// New columns discovered in `other` are null-padded for the existing rows
    /// in `self`.  Columns present in `self` but absent from `other` are
    /// null-padded for `other`'s rows.  Column order follows first-appearance
    /// order across both engines.
    ///
    /// Returns `Err` if any column has a type mismatch between chunks.
    pub fn extend(&mut self, mut other: ColumnarEngine) -> Result<(), String> {
        let self_rows = self.row_count;
        let other_rows = other.row_count;

        // 1. Create columns from other that self doesn't have yet
        //    (null-padded for self's existing rows, no values copied yet)
        //    Use other's plan for type information (self may have BuildPlan::new()).
        //    Respect schema_order for insertion position.
        for name in &other.column_order {
            if !self.column_order.contains(name) {
                let est = self_rows + other.estimated_rows.max(64);
                let col_type = other.plan.column_type(name);
                let mut builder = ColumnBuilder::with_capacity(est, &col_type);
                for _ in 0..self_rows {
                    builder.push(None);
                }
                self.columns.insert(name.clone(), builder);
                let idx = self.schema_insert_index(name);
                self.column_order.insert(idx, name.clone());
            }
        }

        // 2. Append other's values to all columns, null-pad missing ones
        for name in &self.column_order {
            if let Some(self_b) = self.columns.get_mut(name) {
                if let Some(other_b) = other.columns.remove(name) {
                    self_b.extend_owned(other_b)?;
                } else {
                    for _ in 0..other_rows {
                        self_b.push(None);
                    }
                }
            }
        }

        self.row_count = self_rows + other_rows;
        Ok(())
    }

    /// If `auto_dict` is set, upgrade low-cardinality string columns
    /// to dictionary encoding before export.
    pub fn auto_dict_upgrade(&mut self) {
        if self.plan.auto_dict {
            for (_name, b) in self.columns.iter_mut() {
                b.try_upgrade_to_dict(512);
            }
        }
    }

    /// Build a PyArrow table from the columnar data via the Arrow C Data
    /// Interface.  Numeric and dictionary arrays cross the boundary as
    /// contiguous buffers with zero per-cell Python object materialization.
    /// Applies post-reduce filters (column-to-column compare) if any.
    pub fn to_pyarrow_table(&mut self, py: Python<'_>) -> PyResult<PyObject> {
        self.normalize();

        if self.column_order.is_empty() {
            let pa = PyModule::import(py, "pyarrow")?;
            let table: PyObject = pa
                .call_method1("table", (PyDict::new(py),))?
                .into();
            return Ok(table);
        }

        #[cfg(feature = "profile")]
        let _export_start = std::time::Instant::now();

        let mut fields = Vec::with_capacity(self.column_order.len());
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(self.column_order.len());
        for name in &self.column_order {
            if let Some(b) = self.columns.get(name) {
                fields.push(ArrowField::new(name.as_str(), b.arrow_datatype(), true));
                arrays.push(b.to_arrow_array().map_err(PyValueError::new_err)?);
            }
        }

        let schema = Arc::new(Schema::new(fields));
        let batch =
            RecordBatch::try_new(schema, arrays).map_err(|e| PyValueError::new_err(e.to_string()))?;

        // Export the single batch as a pyarrow.RecordBatch via C Data Interface.
        let rb = batch.to_pyarrow(py)?;
        let pa = PyModule::import(py, "pyarrow")?;
        let table: PyObject = pa.call_method1("table", (rb,))?.into();

        #[cfg(feature = "profile")]
        {
            self.profile.export_ns += _export_start.elapsed().as_nanos() as u64;
        }

        // Apply post-reduce filter (column-to-column compare)
        if let Some(ref filter) = self.plan.filter {
            return filter.apply_pyarrow(table, py);
        }
        Ok(table)
    }

    pub fn num_rows(&self) -> usize {
        self.row_count
    }

    pub fn num_columns(&self) -> usize {
        self.column_order.len()
    }

    pub fn column_names(&self) -> &[String] {
        &self.column_order
    }

    /// Sort columns alphabetically so multiple engines produce identical
    /// schemas for pyarrow.concat_tables.
    pub fn sort_columns(&mut self) {
        self.column_order.sort();
    }

    /// Reset all data while preserving the plan and estimated rows.
    pub fn reset(&mut self) {
        self.columns.clear();
        self.column_order.clear();
        self.row_count = 0;
    }

    /// Truncate every column back to `row_count`, dropping any partial-row
    /// values from a mid-field EOF. Idempotent; safe to call before export.
    pub fn normalize(&mut self) {
        for b in self.columns.values_mut() {
            while b.len() > self.row_count {
                b.pop();
            }
        }
    }
}

/// Export per-chunk engines as one pyarrow Table without merging them:
/// each engine becomes a RecordBatch (arrays built in parallel, off-GIL),
/// and the table's columns arrive chunked. This skips the serial
/// merge-then-re-copy of every value that `extend` + `to_pyarrow_table` does.
///
/// Not valid with `auto_dict` (per-chunk upgrades could disagree on the
/// column datatype) — callers must fall back to the merge path there.
pub fn engines_to_pyarrow_table(
    mut engines: Vec<ColumnarEngine>,
    plan: &BuildPlan,
    py: Python<'_>,
) -> PyResult<PyObject> {
    use rayon::prelude::*;

    for e in engines.iter_mut() {
        e.normalize();
    }
    engines.retain(|e| e.row_count > 0);

    // Unified column order + datatypes across chunks. Types are
    // deterministic from the plan, so first sighting wins.
    let mut order: Vec<String> = Vec::new();
    let mut types: HashMap<String, DataType> = HashMap::default();
    for e in &engines {
        for name in &e.column_order {
            if !types.contains_key(name) {
                if let Some(b) = e.columns.get(name) {
                    types.insert(name.clone(), b.arrow_datatype());
                    order.push(name.clone());
                }
            }
        }
    }

    if order.is_empty() {
        let pa = PyModule::import(py, "pyarrow")?;
        return Ok(pa.call_method1("table", (PyDict::new(py),))?.into());
    }

    let fields: Vec<ArrowField> = order
        .iter()
        .map(|n| ArrowField::new(n.as_str(), types[n].clone(), true))
        .collect();
    let schema = Arc::new(Schema::new(fields));

    let batches: Result<Vec<RecordBatch>, String> = engines
        .par_iter()
        .map(|e| {
            let mut arrays: Vec<ArrayRef> = Vec::with_capacity(order.len());
            for name in &order {
                match e.columns.get(name) {
                    Some(b) => arrays.push(b.to_arrow_array()?),
                    None => arrays.push(arrow::array::new_null_array(&types[name], e.row_count)),
                }
            }
            RecordBatch::try_new(schema.clone(), arrays).map_err(|er| er.to_string())
        })
        .collect();
    let batches = batches.map_err(PyValueError::new_err)?;

    let py_batches: Vec<PyObject> = batches
        .iter()
        .map(|b| b.to_pyarrow(py))
        .collect::<PyResult<_>>()?;
    let pa = PyModule::import(py, "pyarrow")?;
    let mut table: PyObject = pa
        .getattr("Table")?
        .call_method1("from_batches", (py_batches,))?
        .into();

    // Per-chunk dictionary arrays carry per-chunk dictionaries; unify them
    // so downstream comparisons see one dictionary. Only pay this when
    // dictionary columns are actually configured.
    if !plan.dictionary_columns.is_empty() {
        table = table.call_method0(py, "combine_chunks")?;
    }

    if let Some(ref filter) = plan.filter {
        return filter.apply_pyarrow(table, py);
    }
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_details() {
        let xml = b"<CrystalReport><Details Level=\"3\"><A>1</A></Details></CrystalReport>";
        let mut engine = ColumnarEngine::new();
        engine.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(engine.num_rows(), 1);
    }

    #[test]
    fn test_parse_two_details() {
        let xml = b"<CrystalReport><Details Level=\"3\"><A>1</A></Details><Details Level=\"3\"><A>2</A></Details></CrystalReport>";
        let mut engine = ColumnarEngine::new();
        engine.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(engine.num_rows(), 2);
    }

    #[test]
    fn test_extend_no_duplicates() {
        let xml = b"<CrystalReport><Details Level=\"3\"><A>1</A></Details></CrystalReport>";
        let mut e1 = ColumnarEngine::new();
        e1.parse_bytes(xml, b"Details").unwrap();

        let mut merged = ColumnarEngine::new();
        merged.extend(e1).unwrap();
        assert_eq!(merged.num_rows(), 1);
        // Verify column data lengths match row_count
        for (name, col) in &merged.columns {
            assert_eq!(col.len(), merged.num_rows(),
                "column {} has {} values but row_count={}", name, col.len(), merged.num_rows());
        }
    }

    #[test]
    fn test_multi_chunk_same_as_single() {
        let xml = b"<CrystalReport><Details Level=\"3\"><A>1</A></Details><Details Level=\"3\"><A>2</A></Details></CrystalReport>";
        let mut single = ColumnarEngine::new();
        single.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(single.num_rows(), 2);

        let row_tag = b"Details";
        let chunks = crate::splitter::compute_splits(xml, row_tag, 2);
        let mut merged = ColumnarEngine::new();
        for chunk in &chunks {
            let mut engine = ColumnarEngine::new();
            engine.parse_bytes(&xml[chunk.clone()], row_tag).unwrap();
            merged.extend(engine).unwrap();
        }
        assert_eq!(merged.num_rows(), single.num_rows(),
            "multi-chunk produced {} rows, expected {} from single",
            merged.num_rows(), single.num_rows());
    }

    #[test]
    fn test_multi_chunk_with_parent_elements() {
        // File has <Group> wrapping some rows — chunk 2 starts mid-Group
        let xml = b"<Root><Group><Details Level=\"3\"><A>1</A></Details><Details Level=\"3\"><A>2</A></Details></Group><Details Level=\"3\"><A>3</A></Details></Root>";
        let mut single = ColumnarEngine::new();
        single.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(single.num_rows(), 3);

        let row_tag = b"Details";
        let chunks = crate::splitter::compute_splits(xml, row_tag, 2);
        assert_eq!(chunks.len(), 2, "expected 2 chunks, got {:?}", chunks);
        let mut merged = ColumnarEngine::new();
        for chunk in &chunks {
            let mut engine = ColumnarEngine::new();
            engine.parse_bytes(&xml[chunk.clone()], row_tag).unwrap();
            merged.extend(engine).unwrap();
        }
        assert_eq!(merged.num_rows(), single.num_rows(),
            "parent-elements test: multi={}, single={}",
            merged.num_rows(), single.num_rows());
    }

    #[test]
    fn test_parallel_same_as_single() {
        use rayon::prelude::*;
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let xml = b"<Root><Group><Details Level=\"3\"><A>1</A></Details><Details Level=\"3\"><A>2</A></Details></Group><Details Level=\"3\"><A>3</A></Details></Root>";
        let mut single = ColumnarEngine::new();
        single.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(single.num_rows(), 3);

        let row_tag = b"Details";
        let chunks = crate::splitter::compute_splits(xml, row_tag, 2);
        let results: Vec<Result<ColumnarEngine, String>> = chunks
            .par_iter()
            .map(|range| {
                catch_unwind(AssertUnwindSafe(|| {
                    let mut engine = ColumnarEngine::new();
                    engine.parse_bytes(&xml[range.clone()], row_tag)?;
                    Ok(engine)
                }))
                .unwrap_or_else(|_| Err("Worker panicked".to_string()))
            })
            .collect();

        let mut merged = ColumnarEngine::new();
        for result in results {
            let engine = result.unwrap();
            merged.extend(engine).unwrap();
        }
        assert_eq!(
            merged.num_rows(),
            single.num_rows(),
            "parallel vs single: multi={}, single={}",
            merged.num_rows(),
            single.num_rows()
        );
    }

    #[test]
    fn test_multi_chunk_single_chunk_fallback() {
        let xml = b"<CrystalReport><Details Level=\"3\"><A>1</A></Details></CrystalReport>";
        let row_tag = b"Details";
        let chunks = crate::splitter::compute_splits(xml, row_tag, 1);
        let mut merged = ColumnarEngine::new();
        for chunk in &chunks {
            let mut engine = ColumnarEngine::new();
            engine.parse_bytes(&xml[chunk.clone()], row_tag).unwrap();
            merged.extend(engine).unwrap();
        }
        assert_eq!(merged.num_rows(), 1,
            "N=1 should produce 1 row, got {}", merged.num_rows());
    }

    #[test]
    fn test_last_write_wins_duplicate_field() {
        // Two <Field Name="X"> in the same row; second value should win.
        let xml = b"<R><Details Level=\"1\"><Section><Field Name=\"Score\"><Value>10</Value></Field><Field Name=\"Score\"><Value>20</Value></Field></Section></Details></R>";
        let mut engine = ColumnarEngine::new();
        engine.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(engine.num_rows(), 1);

        let score_col = engine.columns.get("Score").unwrap();
        assert_eq!(score_col.len(), 1);
        assert_eq!(
            score_col.as_str_vec()[0],
            Some("20".to_string()),
            "last-write-wins: expected '20' got {:?}",
            score_col.as_str_vec()[0]
        );
    }

    #[test]
    fn test_build_plan_rename() {
        let xml = b"<R><Details Level=\"3\"><Section><Field Name=\"Score\"><Value>100</Value></Field></Section></Details></R>";
        let mut plan = BuildPlan::new();
        plan.field_map.insert("Score".to_string(), "Renamed".to_string());
        let mut engine = ColumnarEngine::with_plan(64, plan);
        engine.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(engine.num_rows(), 1);
        assert_eq!(engine.num_columns(), 3); // Level, Section, Renamed
        assert!(engine.columns.contains_key("Renamed"));
        assert!(!engine.columns.contains_key("Score"));
        assert_eq!(engine.columns.get("Renamed").unwrap().as_str_vec()[0], Some("100".into()));
    }

    #[test]
    fn test_build_plan_drop() {
        let xml = b"<R><Details Level=\"3\"><Section><Field Name=\"Score\"><Value>100</Value></Field></Section></Details></R>";
        let mut plan = BuildPlan::new();
        plan.drop_fields.insert("Score".to_string());
        let mut engine = ColumnarEngine::with_plan(64, plan);
        engine.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(engine.num_rows(), 1);
        assert_eq!(engine.num_columns(), 2); // Level + Section
        assert!(!engine.columns.contains_key("Score"));
    }

    #[test]
    fn test_build_plan_filter_ne() {
        // Three rows, second has Score=42 which should be filtered out by !=.
        let xml = b"<R><Details Level=\"3\"><Section><Field Name=\"Score\"><Value>10</Value></Field></Section></Details>\
                       <Details Level=\"2\"><Section><Field Name=\"Score\"><Value>42</Value></Field></Section></Details>\
                       <Details Level=\"1\"><Section><Field Name=\"Score\"><Value>30</Value></Field></Section></Details></R>";
        let mut plan = BuildPlan::new();
        plan.filter = Some(FilterPredicate::NotEqual {
            field: "Score".to_string(),
            value: "42".to_string(),
        });
        let mut engine = ColumnarEngine::with_plan(64, plan);
        engine.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(engine.num_rows(), 2, "filter should keep rows 1 and 3");
        let col = engine.columns.get("Score").unwrap();
        assert_eq!(col.as_str_vec(), &[Some("10".into()), Some("30".into())]);
    }

    #[test]
    fn test_build_plan_filter_eq() {
        let xml = b"<R><Details Level=\"3\"><Section><Field Name=\"Score\"><Value>10</Value></Field></Section></Details>\
                       <Details Level=\"2\"><Section><Field Name=\"Score\"><Value>20</Value></Field></Section></Details>\
                       <Details Level=\"1\"><Section><Field Name=\"Score\"><Value>10</Value></Field></Section></Details></R>";
        let mut plan = BuildPlan::new();
        plan.filter = Some(FilterPredicate::Equal {
            field: "Score".to_string(),
            value: "10".to_string(),
        });
        let mut engine = ColumnarEngine::with_plan(64, plan);
        engine.parse_bytes(xml, b"Details").unwrap();
        assert_eq!(engine.num_rows(), 2, "filter should keep only rows with Score=10");
        let col = engine.columns.get("Score").unwrap();
        assert_eq!(col.as_str_vec(), &[Some("10".into()), Some("10".into())]);
    }

    #[test]
    fn test_build_plan_filter_missing_field() {
        // Second row has no Score field → treated as None (not equal to "10").
        let xml = b"<R><Details Level=\"3\"><Section><Field Name=\"Score\"><Value>10</Value></Field></Section></Details>\
                       <Details Level=\"2\"><Section><Field Name=\"Other\"><Value>99</Value></Field></Section></Details></R>";
        let mut plan = BuildPlan::new();
        plan.filter = Some(FilterPredicate::NotEqual {
            field: "Score".to_string(),
            value: "10".to_string(),
        });
        let mut engine = ColumnarEngine::with_plan(64, plan);
        engine.parse_bytes(xml, b"Details").unwrap();
        // Row 1 has Score=10 → filtered out. Row 2 has no Score → None != "10" → kept.
        assert_eq!(engine.num_rows(), 1);
        let col = engine.columns.get("Level").unwrap();
        assert_eq!(col.as_str_vec(), &[Some("2".into())]);
    }

    #[test]
    fn test_typed_int64_column() {
        let xml = b"<R><Row><Field Name=\"Score\"><Value>42</Value></Field></Row></R>";
        let mut plan = BuildPlan::new();
        plan.field_types
            .insert("Score".to_string(), FieldType::Int64);
        let mut engine = ColumnarEngine::with_plan(64, plan);
        engine.parse_bytes(xml, b"Row").unwrap();
        assert_eq!(engine.num_rows(), 1);
        if let ColumnBuilder::Int64(v) = &engine.columns["Score"] {
            assert_eq!(v[0], Some(42));
        } else {
            panic!("expected Int64 builder");
        }
    }

    #[test]
    fn test_typed_float64_column() {
        let xml = b"<R><Row><Field Name=\"Amount\"><Value>99.5</Value></Field></Row></R>";
        let mut plan = BuildPlan::new();
        plan.field_types
            .insert("Amount".to_string(), FieldType::Float64);
        let mut engine = ColumnarEngine::with_plan(64, plan);
        engine.parse_bytes(xml, b"Row").unwrap();
        if let ColumnBuilder::Float64(v) = &engine.columns["Amount"] {
            assert!((v[0].unwrap() - 99.5).abs() < 1e-9);
        } else {
            panic!("expected Float64 builder");
        }
    }

    #[test]
    fn test_typed_parse_failure_nulls() {
        let xml = b"<R><Row><Field Name=\"Score\"><Value>42</Value></Field></Row>\
                        <Row><Field Name=\"Score\"><Value>N/A</Value></Field></Row>\
                        <Row><Field Name=\"Score\"><Value>100</Value></Field></Row></R>";
        let mut plan = BuildPlan::new();
        plan.field_types
            .insert("Score".to_string(), FieldType::Int64);
        let mut engine = ColumnarEngine::with_plan(64, plan);
        engine.parse_bytes(xml, b"Row").unwrap();
        assert_eq!(engine.num_rows(), 3);
        if let ColumnBuilder::Int64(v) = &engine.columns["Score"] {
            assert_eq!(v[0], Some(42));
            assert_eq!(v[1], None);
            assert_eq!(v[2], Some(100));
        } else {
            panic!("expected Int64 builder");
        }
    }

    #[test]
    fn test_dictionary_column() {
        let xml = b"<R><Row><Field Name=\"Product\"><Value>Widget</Value></Field></Row>\
                        <Row><Field Name=\"Product\"><Value>Gadget</Value></Field></Row>\
                        <Row><Field Name=\"Product\"><Value>Widget</Value></Field></Row></R>";
        let mut plan = BuildPlan::new();
        plan.dictionary_columns.insert("Product".to_string());
        let mut engine = ColumnarEngine::with_plan(64, plan);
        engine.parse_bytes(xml, b"Row").unwrap();
        assert_eq!(engine.num_rows(), 3);
        if let ColumnBuilder::Dictionary { codes, dict, .. } = &engine.columns["Product"] {
            assert_eq!(dict.len(), 2); // Widget, Gadget
            assert_eq!(codes[0], Some(0)); // Widget
            assert_eq!(codes[1], Some(1)); // Gadget
            assert_eq!(codes[2], Some(0)); // Widget again
        } else {
            panic!("expected Dictionary builder");
        }
    }

    // ── Ground-truth oracle (independent of columnar storage) ──────────────

    /// Walk raw XML bytes and extract row-major key-value pairs.
    /// Shares no code with `ColumnarEngine` — used as an independent reference
    /// for multi-chunk / parallel test assertions.
    fn row_values_reference(bytes: &[u8], row_tag: &[u8]) -> Vec<std::collections::HashMap<String, String>> {
        use quick_xml::events::{BytesStart, Event};
        use quick_xml::Reader;
        use std::collections::HashMap;
        use std::io::Cursor;

        let mut reader = Reader::from_reader(Cursor::new(bytes));
        reader.config_mut().check_end_names = false;
        let mut buf = Vec::new();
        let mut rows = Vec::new();

        fn read_child_text(
            reader: &mut Reader<Cursor<&[u8]>>,
            end: &[u8],
            wanted: &[&[u8]],
        ) -> String {
            let mut text = String::new();
            let mut ibuf = Vec::new();
            loop {
                match reader.read_event_into(&mut ibuf) {
                    Ok(Event::Start(ref ic)) => {
                        if wanted.contains(&ic.name().as_ref()) {
                            if let Ok(Event::Text(t)) = reader.read_event_into(&mut ibuf) {
                                if let Ok(v) = t.unescape() {
                                    text = v.into_owned();
                                }
                            }
                        }
                    }
                    Ok(Event::Empty(_)) => {}
                    Ok(Event::End(ref e)) if e.name().as_ref() == end => break,
                    Ok(Event::Eof) | Err(_) => break,
                    _ => {}
                }
                ibuf.clear();
            }
            text
        }

        fn attr(e: &BytesStart, keys: &[&[u8]]) -> Option<String> {
            e.attributes()
                .flatten()
                .find(|a| keys.contains(&a.key.as_ref()))
                .and_then(|a| a.unescape_value().ok())
                .map(|v| v.into_owned())
        }

        loop {
            let event = match reader.read_event_into(&mut buf) {
                Ok(e) => e,
                Err(_) => break,
            };

            let is_row_empty = matches!(&event, Event::Empty(e) if e.name().as_ref() == row_tag);
            let is_row_start = matches!(&event, Event::Start(e) if e.name().as_ref() == row_tag);

            if is_row_empty {
                if let Event::Empty(ref e) = event {
                    let mut row = HashMap::default();
                    for a in e.attributes().flatten() {
                        let k = String::from_utf8_lossy(a.key.as_ref()).into_owned();
                        row.insert(k, a.unescape_value().unwrap_or_default().into_owned());
                    }
                    rows.push(row);
                }
                buf.clear();
                continue;
            }

            if !is_row_start {
                if matches!(event, Event::Eof) {
                    break;
                }
                buf.clear();
                continue;
            }

            // Row Start event: capture attributes + children
            let mut row = HashMap::default();
            if let Event::Start(ref e) = event {
                for a in e.attributes().flatten() {
                    let k = String::from_utf8_lossy(a.key.as_ref()).into_owned();
                    row.insert(k, a.unescape_value().unwrap_or_default().into_owned());
                }
            }

            let mut cbuf = Vec::new();
            loop {
                match reader.read_event_into(&mut cbuf) {
                    Ok(Event::Start(ref c)) => {
                        let tag = c.name().as_ref().to_vec();
                        if tag == b"Field" {
                            let key = attr(c, &[b"FieldName", b"Name"]).unwrap_or_else(|| "Field".into());
                            let val = read_child_text(&mut reader, b"Field", &[b"FormattedValue", b"Value"]);
                            row.insert(key, val);
                        } else if tag == b"Text" {
                            let key = attr(c, &[b"Name"]).unwrap_or_else(|| "Text".into());
                            let val = read_child_text(&mut reader, b"Text", &[b"TextValue"]);
                            row.insert(key, val);
                        } else if tag == b"Section" {
                            let sn = attr(c, &[b"SectionNumber"]).unwrap_or_default();
                            row.insert("Section".into(), sn);
                        } else {
                            let key = String::from_utf8_lossy(&tag).into_owned();
                            row.insert(key, String::new());
                        }
                    }
                    Ok(Event::Empty(ref c)) => {
                        let tag = c.name().as_ref().to_vec();
                        if tag == b"Field" {
                            let key = attr(c, &[b"FieldName", b"Name"]).unwrap_or_else(|| "Field".into());
                            row.insert(key, String::new());
                        } else if tag == b"Text" {
                            let key = attr(c, &[b"Name"]).unwrap_or_else(|| "Text".into());
                            row.insert(key, String::new());
                        } else if tag == b"Section" {
                            let sn = attr(c, &[b"SectionNumber"]).unwrap_or_default();
                            row.insert("Section".into(), sn);
                        } else {
                            let key = String::from_utf8_lossy(&tag).into_owned();
                            row.insert(key, String::new());
                        }
                    }
                    Ok(Event::End(ref e)) if e.name().as_ref() == row_tag => break,
                    Ok(Event::Eof) | Err(_) => break,
                    _ => {}
                }
                cbuf.clear();
            }

            rows.push(row);
            buf.clear();
        }
        rows
    }

    /// Reconstruct row-major hash maps from columnar engine state.
    fn row_values(engine: &ColumnarEngine) -> Vec<std::collections::HashMap<String, String>> {
        use std::collections::HashMap;
        (0..engine.row_count)
            .map(|i| {
                let mut m = HashMap::default();
                for name in &engine.column_order {
                    if let Some(b) = engine.columns.get(name) {
                        if let Some(s) = b.get_filter_value(i) {
                            m.insert(name.clone(), s);
                        }
                    }
                }
                m
            })
            .collect()
    }

    // ── A2: Ragged late-chunk column debut ────────────────────────────────

    #[test]
    fn test_ragged_late_chunk_column_debut() {
        let full = b"<R>\
            <Details A=\"1\"><Field Name=\"A\"><Value>1</Value></Field><Field Name=\"B\"><Value>2</Value></Field></Details>\
            <Details A=\"3\"><Field Name=\"A\"><Value>3</Value></Field></Details>\
            <Details><Field Name=\"B\"><Value>4</Value></Field><Field Name=\"C\"><Value>5</Value></Field></Details>\
            </R>";
        let chunk1 = b"<R>\
            <Details A=\"1\"><Field Name=\"A\"><Value>1</Value></Field><Field Name=\"B\"><Value>2</Value></Field></Details>\
            <Details A=\"3\"><Field Name=\"A\"><Value>3</Value></Field></Details>";
        let chunk2 = b"<Details><Field Name=\"B\"><Value>4</Value></Field><Field Name=\"C\"><Value>5</Value></Field></Details></R>";
        let tag = b"Details";

        let mut single = ColumnarEngine::new();
        single.parse_bytes(full, tag).unwrap();

        let mut e1 = ColumnarEngine::new();
        e1.parse_bytes(chunk1, tag).unwrap();
        let mut e2 = ColumnarEngine::new();
        e2.parse_bytes(chunk2, tag).unwrap();

        let mut merged = ColumnarEngine::new();
        merged.extend(e1).unwrap();
        merged.extend(e2).unwrap();

        // Locked column arrays (simulation-verified).
        assert_eq!(
            merged.columns["A"].as_str_vec(),
            &[Some("1".into()), Some("3".into()), None]
        );
        assert_eq!(
            merged.columns["B"].as_str_vec(),
            &[Some("2".into()), None, Some("4".into())]
        );
        assert_eq!(
            merged.columns["C"].as_str_vec(),
            &[None, None, Some("5".into())]
        );

        // Multi == single == independent oracle.
        let oracle = row_values_reference(full, tag);
        assert_eq!(
            row_values(&single),
            oracle,
            "single-chunk must match oracle"
        );
        assert_eq!(
            row_values(&merged),
            oracle,
            "merged multi-chunk must match oracle"
        );
    }

    // ── A3: Mid-field EOF truncation ──────────────────────────────────────

    #[test]
    fn test_midfield_eof_truncation_discards_partial_row() {
        // Chunk ends mid-<Field>: partial row must be discarded.
        let chunk1 = b"<R><Details><Field Name=\"A\"><Value>1</Value></Field></Details>";
        let chunk2 = b"<Details><Field Name=\"C\"><Value>5</Value>"; // truncated
        let tag = b"Details";

        let mut e2 = ColumnarEngine::new();
        e2.parse_bytes(chunk2, tag).unwrap();
        e2.normalize();
        assert_eq!(e2.num_rows(), 0, "partial row must not be counted");
        for name in e2.column_names() {
            assert_eq!(
                e2.columns[name].len(),
                e2.num_rows(),
                "column {} ragged after normalize",
                name
            );
        }

        let mut e1 = ColumnarEngine::new();
        e1.parse_bytes(chunk1, tag).unwrap();
        let mut merged = ColumnarEngine::new();
        merged.extend(e1).unwrap();
        merged.extend(e2).unwrap();
        merged.normalize();
        assert_eq!(merged.num_rows(), 1);
        for name in merged.column_names() {
            assert_eq!(merged.columns[name].len(), merged.num_rows());
        }
    }

    // ── A4: auto_dict upgrade order ───────────────────────────────────────

    #[test]
    fn test_auto_dict_upgrade_only_post_merge() {
        let xml = b"<R><Row><Field Name=\"P\"><Value>x</Value></Field></Row>\
                        <Row><Field Name=\"P\"><Value>y</Value></Field></Row></R>";
        let tag = b"Row";
        let mut plan = BuildPlan::new();
        plan.auto_dict = true;

        let mut a = ColumnarEngine::with_plan(64, plan.clone());
        a.parse_bytes(xml, tag).unwrap();
        let mut b = ColumnarEngine::with_plan(64, plan.clone());
        b.parse_bytes(xml, tag).unwrap();

        let mut merged = ColumnarEngine::with_plan(64, plan);
        merged.extend(a).unwrap();
        merged.extend(b).unwrap();
        merged.auto_dict_upgrade(); // post-merge only; must not panic
        assert_eq!(merged.num_rows(), 4);
    }

    // ── B: extend variant mismatch → Err, not panic ───────────────────────

    #[test]
    fn test_extend_variant_mismatch_errors_not_panics() {
        let xml = b"<R><Row><Field Name=\"P\"><Value>x</Value></Field></Row></R>";
        let tag = b"Row";

        let mut e1 = ColumnarEngine::new();
        e1.parse_bytes(xml, tag).unwrap(); // P as String

        let mut plan = BuildPlan::new();
        plan.dictionary_columns.insert("P".to_string());
        let mut e2 = ColumnarEngine::with_plan(64, plan);
        e2.parse_bytes(xml, tag).unwrap(); // P as Dictionary

        let result = e1.extend(e2);
        assert!(result.is_err(), "String/Dictionary mismatch must return Err");
    }

    #[test]
    fn test_malformed_xml_errors_loudly() {
        // Unterminated attribute quote is real corruption, not a chunk seam.
        let xml = b"<R><Row><Field Name=\"Score><Value>10</Value></Field></Row></R>";
        let mut engine = ColumnarEngine::new();
        assert!(engine.parse_bytes(xml, b"Row").is_err());
    }
}
