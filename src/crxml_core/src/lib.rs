#![deny(unsafe_code)]

use arrow::pyarrow::ToPyArrow;
use arrow::record_batch::RecordBatch;
use pyo3::exceptions::{PyException, PyIOError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyString};
use pyo3::wrap_pyfunction;
use quick_xml::events::Event;
use quick_xml::Reader;
use rypipe_core::RecordParser;
use rypipe_core::Splitter;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::ops::Range;
use std::path::Path;
#[cfg(feature = "profile")]
use std::time::Instant;

/// Auto-enable mmap for large uncompressed files (>50 MB) to avoid
/// `std::fs::read` copy (~3–10% of wall per `perf`). Respects explicit
/// `use_mmap=true`; when `use_mmap=false` and file is large, checks magic
/// bytes for gzip/zstd/lz4 and only enables mmap for uncompressed.
fn auto_mmap(path: &Path, use_mmap: bool) -> bool {
    if use_mmap {
        return true;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.len() <= 50 * 1024 * 1024 {
        return false;
    }
    // Check compression magic before enabling mmap; compressed files are
    // decompressed into Owned via InputBuffer::detect_compression anyway.
    if let Ok(mut f) = File::open(path) {
        use std::io::Read;
        let mut buf = [0u8; 4];
        let n = f.read(&mut buf).unwrap_or(0);
        if n >= 2 && buf[0..2] == [0x1f, 0x8b] {
            return false; // gzip
        }
        if n >= 4 && (buf == [0x28, 0xb5, 0x2f, 0xfd] || buf == [0x04, 0x22, 0x4d, 0x18]) {
            return false; // zstd / lz4
        }
    }
    true
}

mod xml;

// Fast allocator: replaces the system heap for all Rust-side
// allocations (profiling showed ~27% of CPU in malloc/free).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// Typed exceptions so callers can distinguish failure classes:
// XmlError: malformed/unparseable XML input; PlanError: invalid pushdown
// plan kwargs (bad ops, unknown types); MergeError: chunk-merge conflicts.
pyo3::create_exception!(crxml, XmlError, PyException);
pyo3::create_exception!(crxml, PlanError, PyException);
pyo3::create_exception!(crxml, MergeError, PyException);

fn map_rypipe_err(e: rypipe_core::Error) -> PyErr {
    match e {
        rypipe_core::Error::Utf8(_) => XmlError::new_err(format!("Columnar parse error: {}", e)),
        rypipe_core::Error::Plan(ref msg)
            if msg.starts_with("XML parse error") || msg.starts_with("invalid UTF-8") =>
        {
            XmlError::new_err(format!("Columnar parse error: {}", msg))
        }
        rypipe_core::Error::Plan(msg) => PlanError::new_err(msg),
        rypipe_core::Error::Merge(msg) => MergeError::new_err(msg),
        rypipe_core::Error::Io(io) => PyIOError::new_err(io.to_string()),
        rypipe_core::Error::Arrow(a) => PyException::new_err(format!("Arrow error: {}", a)),
    }
}

fn build_plan_from_kwargs(
    field_mapping: Option<HashMap<String, String>>,
    drop_fields: Option<Vec<String>>,
    filter: Option<HashMap<String, String>>,
    field_types: Option<HashMap<String, String>>,
    dictionary_columns: Option<Vec<String>>,
    schema: Option<Vec<String>>,
    auto_dict: bool,
) -> PyResult<rypipe_core::ExecutionPlan> {
    let mut plan = rypipe_core::ExecutionPlan::new();

    if let Some(map) = field_mapping {
        plan.field_map = map.into_iter().collect();
    }

    if let Some(drop) = drop_fields {
        plan.drop_fields = drop.into_iter().collect();
    }

    if let Some(s) = schema {
        plan.schema_order = s;
    }

    plan.auto_dict = auto_dict;

    if let Some(ft) = field_types {
        for (name, type_str) in ft {
            let ft = rypipe_core::FieldType::from_str(&type_str).ok_or_else(|| {
                let valid = "string, int64, float64, bool";
                PyException::new_err(format!(
                    "unknown field type '{type_str}' for '{name}'; \
                         valid types: {valid}"
                ))
            })?;
            plan.field_types.insert(name, ft);
        }
    }

    if let Some(dict) = dictionary_columns {
        plan.dictionary_columns = dict.into_iter().collect();
    }

    if let Some(f) = filter {
        let op = f
            .get("op")
            .ok_or_else(|| PlanError::new_err("filter must include 'op' key"))?
            .to_owned();
        // Column-to-column filter: field_a + op + field_b
        if f.contains_key("field_a") && f.contains_key("field_b") {
            let field_a = f.get("field_a").unwrap().to_owned();
            let field_b = f.get("field_b").unwrap().to_owned();
            let cop = rypipe_core::CompareOp::from_str(&op).ok_or_else(|| {
                let valid = ">, <, >=, <=, ==, !=";
                PlanError::new_err(format!("unsupported compare op {op:?}; valid: {valid}"))
            })?;
            plan.filter = Some(rypipe_core::FilterPredicate::Compare {
                field_a,
                op: cop,
                field_b,
            });
        } else {
            let field = f
                .get("field")
                .ok_or_else(|| PlanError::new_err("filter must include 'field' key"))?
                .to_owned();
            let value = f
                .get("value")
                .ok_or_else(|| PlanError::new_err("filter must include 'value' key"))?
                .to_owned();
            plan.filter = Some(match op.as_str() {
                "!=" | "ne" => rypipe_core::FilterPredicate::NotEqual { field, value },
                "==" | "eq" => rypipe_core::FilterPredicate::Equal { field, value },
                other => {
                    let msg = format!("unsupported filter op {other:?}; use '!=' or '=='");
                    return Err(PlanError::new_err(msg));
                }
            });
        }
    }

    Ok(plan)
}

fn split_points_to_ranges(points: &[usize], len: usize) -> Vec<Range<usize>> {
    if points.len() < 2 {
        return vec![0..len];
    }
    points
        .windows(2)
        .filter_map(|w| {
            let (start, end) = (w[0], w[1]);
            if start < end {
                Some(start..end)
            } else {
                None
            }
        })
        .collect()
}

fn empty_table(py: Python<'_>) -> PyResult<PyObject> {
    let pa = PyModule::import(py, "pyarrow")?;
    Ok(pa.call_method1("table", (PyDict::new(py),))?.into())
}

fn record_batch_to_table(batch: RecordBatch, py: Python<'_>) -> PyResult<PyObject> {
    let pa = PyModule::import(py, "pyarrow")?;
    // An empty-schema RecordBatch cannot be round-tripped through
    // Table.from_batches; return an empty table instead.
    if batch.schema().fields().is_empty() {
        return Ok(empty_table(py)?);
    }
    let rb = batch.to_pyarrow(py)?;
    let table = pa
        .getattr("Table")?
        .call_method1("from_batches", (PyList::new(py, vec![rb])?,))?;
    Ok(table.into())
}

fn concat_tables(a: PyObject, b: PyObject, py: Python<'_>) -> PyResult<PyObject> {
    let pa = PyModule::import(py, "pyarrow")?;
    let tables_list = PyList::new(py, vec![a, b])?;

    // Fast path: schemas match; no promotion needed.
    let a_schema = tables_list.get_item(0)?.getattr("schema")?;
    let b_schema = tables_list.get_item(1)?.getattr("schema")?;
    let schemas_match = a_schema.call_method1("__eq__", (b_schema,))?.is_truthy()?;

    if schemas_match {
        return Ok(pa.call_method1("concat_tables", (tables_list,))?.into());
    }

    // Schemas differ (column order or auto_dict promotion): use promote.
    let kwargs = PyDict::new(py);
    kwargs.set_item("promote_options", "default")?;
    Ok(pa
        .call_method("concat_tables", (tables_list,), Some(&kwargs))?
        .into())
}

fn record_batches_to_table(batches: Vec<RecordBatch>, py: Python<'_>) -> PyResult<PyObject> {
    let table = if batches.is_empty() {
        let pa = PyModule::import(py, "pyarrow")?;
        pa.call_method1("table", (PyDict::new(py),))?.into()
    } else {
        let mut result_table: Option<PyObject> = None;
        for batch in batches {
            let t = record_batch_to_table(batch, py)?;
            result_table = match result_table {
                None => Some(t),
                Some(prev) => Some(concat_tables(prev, t, py)?),
            };
        }
        result_table.unwrap()
    };
    // Flatten chunked tables so that parallel/bounded outputs compare equal
    // to single-batch tables produced by the single-chunk path.
    Ok(table.call_method0(py, "combine_chunks")?.into())
}

#[pyfunction]
#[pyo3(signature = (path, row_tag=None, field_mapping=None, drop_fields=None, filter=None, field_types=None, dictionary_columns=None, use_mmap=false, schema=None, auto_dict=false, prefault=false))]
pub fn read_to_columnar(
    path: String,
    row_tag: Option<String>,
    field_mapping: Option<HashMap<String, String>>,
    drop_fields: Option<Vec<String>>,
    filter: Option<HashMap<String, String>>,
    field_types: Option<HashMap<String, String>>,
    dictionary_columns: Option<Vec<String>>,
    use_mmap: bool,
    schema: Option<Vec<String>>,
    auto_dict: bool,
    prefault: bool,
) -> PyResult<PyObject> {
    let plan = build_plan_from_kwargs(
        field_mapping,
        drop_fields,
        filter,
        field_types,
        dictionary_columns,
        schema,
        auto_dict,
    )?;

    let p = Path::new(&path);
    if !p.is_file() {
        return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
    }
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string());

    let mmap = auto_mmap(p, use_mmap);
    let input = rypipe_core::InputBuffer::open(p, mmap, prefault).map_err(map_rypipe_err)?;
    let bytes = input.as_slice();
    let decoder = crate::xml::CrystalXmlDecoder::with_row_tag(&row_tag);
    // Use splitter's estimate for capacity instead of constant 512.
    let est_row = crate::xml::CrystalXmlSplitter::with_row_tag(&row_tag)
        .estimate_bytes_per_row(&bytes[..bytes.len().min(65536)]);
    let cap = (bytes.len() / est_row.max(512)).max(64);
    let mut table_builder = rypipe_core::TableBuilder::with_plan(cap, plan.clone());
    decoder.validate(bytes).map_err(map_rypipe_err)?;
    decoder
        .parse_chunk(bytes, &mut table_builder)
        .map_err(map_rypipe_err)?;

    if table_builder.num_columns() == 0 {
        return Python::with_gil(|py| empty_table(py));
    }

    let mut batch = table_builder.finish().map_err(map_rypipe_err)?;
    if let Some(ref filter) = plan.filter {
        if let rypipe_core::FilterPredicate::Compare { .. } = filter {
            batch = rypipe_core::apply_compare_filter(batch, filter).map_err(map_rypipe_err)?;
        }
    }

    Python::with_gil(|py| record_batch_to_table(batch, py))
}

#[pyfunction]
#[pyo3(signature = (path, row_tag=None, num_chunks=2, field_mapping=None, drop_fields=None, filter=None, field_types=None, dictionary_columns=None, use_mmap=false, schema=None, auto_dict=false, prefault=false))]
pub fn read_to_columnar_multi(
    path: String,
    row_tag: Option<String>,
    num_chunks: usize,
    field_mapping: Option<HashMap<String, String>>,
    drop_fields: Option<Vec<String>>,
    filter: Option<HashMap<String, String>>,
    field_types: Option<HashMap<String, String>>,
    dictionary_columns: Option<Vec<String>>,
    use_mmap: bool,
    schema: Option<Vec<String>>,
    auto_dict: bool,
    prefault: bool,
) -> PyResult<PyObject> {
    let plan = build_plan_from_kwargs(
        field_mapping,
        drop_fields,
        filter,
        field_types,
        dictionary_columns,
        schema,
        auto_dict,
    )?;

    let p = Path::new(&path);
    if !p.is_file() {
        return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
    }
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string());

    let mmap = auto_mmap(p, use_mmap);
    let input = rypipe_core::InputBuffer::open(p, mmap, prefault).map_err(map_rypipe_err)?;
    let bytes = input.as_slice();
    let splitter = crate::xml::CrystalXmlSplitter::with_row_tag(&row_tag);
    let decoder = crate::xml::CrystalXmlDecoder::with_row_tag(&row_tag);
    let split_points = splitter.find_split_points(bytes, num_chunks);
    let ranges = split_points_to_ranges(&split_points, bytes.len());

    let est_row = splitter.estimate_bytes_per_row(&bytes[..bytes.len().min(65536)]);
    let cap = (bytes.len() / est_row.max(512)).max(64);
    let mut merged = rypipe_core::TableBuilder::with_plan(cap, plan.clone());
    for range in ranges {
        let mut sink =
            rypipe_core::TableBuilder::with_plan((range.len() / est_row.max(512)).max(64), plan.clone());
        decoder.validate(&bytes[range.clone()]).map_err(|e| {
            XmlError::new_err(format!("Columnar parse error in chunk {:?}: {}", range, e))
        })?;
        decoder
            .parse_chunk(&bytes[range.clone()], &mut sink)
            .map_err(|e| {
                XmlError::new_err(format!("Columnar parse error in chunk {:?}: {}", range, e))
            })?;
        merged
            .extend(sink)
            .map_err(|e| MergeError::new_err(e.to_string()))?;
    }

    if merged.num_columns() == 0 {
        return Python::with_gil(|py| empty_table(py));
    }

    let mut batch = merged.finish().map_err(map_rypipe_err)?;
    if let Some(ref filter) = plan.filter {
        if let rypipe_core::FilterPredicate::Compare { .. } = filter {
            batch = rypipe_core::apply_compare_filter(batch, filter).map_err(map_rypipe_err)?;
        }
    }

    Python::with_gil(|py| record_batch_to_table(batch, py))
}

#[pyfunction]
#[pyo3(signature = (path, row_tag=None, num_chunks=4, field_mapping=None, drop_fields=None, filter=None, field_types=None, dictionary_columns=None, use_mmap=false, schema=None, auto_dict=false, prefault=false))]
pub fn read_to_columnar_par(
    path: String,
    row_tag: Option<String>,
    num_chunks: usize,
    field_mapping: Option<HashMap<String, String>>,
    drop_fields: Option<Vec<String>>,
    filter: Option<HashMap<String, String>>,
    field_types: Option<HashMap<String, String>>,
    dictionary_columns: Option<Vec<String>>,
    use_mmap: bool,
    schema: Option<Vec<String>>,
    auto_dict: bool,
    prefault: bool,
) -> PyResult<PyObject> {
    let plan = build_plan_from_kwargs(
        field_mapping,
        drop_fields,
        filter,
        field_types,
        dictionary_columns,
        schema,
        auto_dict,
    )?;

    let p = Path::new(&path);
    if !p.is_file() {
        return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
    }
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string());

    let mmap = auto_mmap(p, use_mmap);
    let input = rypipe_core::InputBuffer::open(p, mmap, prefault).map_err(map_rypipe_err)?;
    let bytes = input.as_slice();
    let splitter = crate::xml::CrystalXmlSplitter::with_row_tag(&row_tag);
    let decoder = crate::xml::CrystalXmlDecoder::with_row_tag(&row_tag);
    let batches =
        rypipe_core::parallel::ParallelExecutor::parse(bytes, &splitter, decoder, plan, num_chunks)
            .map_err(map_rypipe_err)?;

    Python::with_gil(|py| record_batches_to_table(batches, py))
}

#[pyfunction]
#[pyo3(signature = (path, row_tag, memory, field_mapping=None, drop_fields=None, filter=None, field_types=None, dictionary_columns=None, schema=None, auto_dict=false, prefault=false))]
pub fn read_to_columnar_bounded(
    path: String,
    row_tag: Option<String>,
    memory: usize,
    field_mapping: Option<HashMap<String, String>>,
    drop_fields: Option<Vec<String>>,
    filter: Option<HashMap<String, String>>,
    field_types: Option<HashMap<String, String>>,
    dictionary_columns: Option<Vec<String>>,
    schema: Option<Vec<String>>,
    auto_dict: bool,
    prefault: bool,
) -> PyResult<PyObject> {
    let plan = build_plan_from_kwargs(
        field_mapping,
        drop_fields,
        filter,
        field_types,
        dictionary_columns,
        schema,
        auto_dict,
    )?;
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string());

    let budget = rypipe_core::bounded::MemoryBudget::new(memory);
    let executor = rypipe_core::bounded::BoundedExecutor::new(budget);
    let splitter = crate::xml::CrystalXmlSplitter::with_row_tag(&row_tag);
    let decoder = crate::xml::CrystalXmlDecoder::with_row_tag(&row_tag);
    let batches = executor
        .run(Path::new(&path), &splitter, decoder, plan, prefault)
        .map_err(map_rypipe_err)?;

    Python::with_gil(|py| record_batches_to_table(batches, py))
}

#[cfg(feature = "profile")]
#[pyfunction]
fn get_par_profile(py: Python<'_>) -> PyResult<PyObject> {
    let d = PyDict::new(py);
    d.set_item("split_scan_ns", 0u64)?;
    d.set_item("parse_ns", 0u64)?;
    d.set_item("assembly_export_ns", 0u64)?;
    Ok(d.into())
}

#[cfg(feature = "testing")]
fn _run_parser(bytes: &[u8], row_tag: &[u8]) -> PyResult<PyObject> {
    let plan = rypipe_core::ExecutionPlan::new();
    let est = (bytes.len() / 512).max(64);
    let mut sink = rypipe_core::TableBuilder::with_plan(est, plan);
    let decoder = crate::xml::CrystalXmlDecoder::with_row_tag(row_tag);
    decoder.validate(bytes).map_err(map_rypipe_err)?;
    decoder
        .parse_chunk(bytes, &mut sink)
        .map_err(map_rypipe_err)?;
    let batch = sink.finish().map_err(map_rypipe_err)?;
    Python::with_gil(|py| record_batch_to_table(batch, py))
}

/// Testing helper: parse bytes with the columnar engine.
/// Returns the exported pyarrow table, or raises on parse failure.
#[cfg(feature = "testing")]
#[pyfunction]
#[pyo3(signature = (bytes, row_tag=None))]
fn _test_parse_both(bytes: Vec<u8>, row_tag: Option<String>) -> PyResult<PyObject> {
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string());
    _run_parser(&bytes, row_tag.as_bytes())
}

/// Testing helper: parse bytes (identical to _test_parse_both).
/// Kept for backward compatibility with benchmarks that reference it.
#[cfg(feature = "testing")]
#[pyfunction]
#[pyo3(signature = (bytes, row_tag=None))]
fn _test_parse_fast(bytes: Vec<u8>, row_tag: Option<String>) -> PyResult<PyObject> {
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string());
    _run_parser(&bytes, row_tag.as_bytes())
}

/// Testing helper: parse bytes with the columnar engine.
#[cfg(feature = "testing")]
#[pyfunction]
#[pyo3(signature = (bytes, row_tag=None))]
fn _test_parse_quickxml(bytes: Vec<u8>, row_tag: Option<String>) -> PyResult<PyObject> {
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string());
    _run_parser(&bytes, row_tag.as_bytes())
}

#[cfg(feature = "profile")]
#[derive(Default, Clone)]
pub struct ProfileCounters {
    /// Cumulative nanosecond counter for quick-xml event loop (read_event_into).
    pub event_loop_ns: u64,
    /// Cumulative nanosecond counter for unescape_cow calls.
    pub unescape_ns: u64,
    /// Cumulative nanosecond counter for Python dict construction.
    pub dict_build_ns: u64,
}

#[cfg(feature = "profile")]
macro_rules! measure_el {
    ($profile:expr, $field:ident, $body:expr) => {{
        let __start = Instant::now();
        let __result = $body;
        $profile.$field += __start.elapsed().as_nanos() as u64;
        __result
    }};
}

#[cfg(not(feature = "profile"))]
macro_rules! measure_el {
    ($profile:expr, $field:ident, $body:expr) => {
        $body
    };
}

/// Pure-Rust parsing state for the stream engine (super-optimized).
///
/// Uses the same `memchr` scanner as columnar (`crate::xml::scanner`) but
/// yields rows one-by-one via a `RowSink` instead of `TableBuilder`. Holds
/// `InputBuffer` (mmap or owned) like columnar for zero-copy, and reuses
/// `find_special_regions` / `next_row_start` for split-scan quality.
///
/// # Load-bearing invariants
/// - Holds **no** `Py<...>` objects. This is required for `py.allow_threads`
///   to compile (the `Ungil` bound on the closure). Python-object state (the
///   interned-key cache) lives on `CrxmlReader` beside this struct, and only
///   the GIL-held dict-build code touches it.
struct RowParser {
    input: rypipe_core::InputBuffer,
    pos: usize,
    regions: Vec<Range<usize>>,
    row_tag: Vec<u8>,
    /// Per-row scratch buffer (cleared each row, retains capacity).
    row: Vec<(String, String)>,
    /// Flat field buffer for batched output. Grows to fit one batch.
    batch_vals: Vec<(String, String)>,
    /// Field count per row in `batch_vals`, for slicing.
    batch_lens: Vec<usize>,
    #[cfg(feature = "profile")]
    profile: ProfileCounters,
}

/// Adapter sink for streaming: pushes `Value::Str` directly into `RowParser::row`
/// without `TableBuilder`'s `FxHashMap` / arena / `row_dirty` overhead. Used
/// only by `RowParser::read_one_row` / `read_batch_into`.
struct RowSink<'a> {
    row: &'a mut Vec<(String, String)>,
}

impl<'a> rypipe_core::ColumnarSink for RowSink<'a> {
    fn begin_row(&mut self) {
        self.row.clear();
    }
    fn put_field(&mut self, name: &str, value: rypipe_core::Value<'_>) {
        // `scanner` only emits `Value::Str`; other variants are coerced via `to_string`.
        let v = match value {
            rypipe_core::Value::Str(s) => s.to_string(),
            rypipe_core::Value::Int64(i) => i.to_string(),
            rypipe_core::Value::Float64(f) => f.to_string(),
            rypipe_core::Value::Bool(b) => b.to_string(),
            _ => String::new(),
        };
        self.row.push((name.to_string(), v));
    }
    fn end_row(&mut self) {}
    fn finish(&mut self) -> rypipe_core::Result<RecordBatch> {
        Ok(RecordBatch::new_empty(std::sync::Arc::new(arrow::datatypes::Schema::empty())))
    }
}

/// Streaming CR XML row parser exposed to Python.
#[pyclass]
pub struct CrxmlReader {
    parser: RowParser,
    /// Field names repeat identically every row; interning the PyString per
    /// key lets every dict reuse the same object instead of building a fresh
    /// PyUnicode per field per row.
    key_cache: rustc_hash::FxHashMap<String, Py<PyString>>,
}

/// Look up (or create) the interned PyString for `key`.
fn cached_key<'a>(
    py: Python<'_>,
    cache: &'a mut rustc_hash::FxHashMap<String, Py<PyString>>,
    key: &str,
) -> &'a Py<PyString> {
    if !cache.contains_key(key) {
        cache.insert(key.to_owned(), PyString::new(py, key).unbind());
    }
    &cache[key]
}

fn new_dict(py: Python<'_>) -> Bound<'_, PyDict> {
    PyDict::new(py)
}

// Pure-Rust helpers (not #[pymethods]): no Python objects touched.
impl RowParser {
    fn read_one_row(&mut self) -> Result<Option<usize>, String> {
        self.row.clear();
        let bytes = self.input.as_slice();
        let mut sink = RowSink { row: &mut self.row };
        match crate::xml::scanner::scan_one_row(bytes, self.pos, &self.row_tag, &self.regions, &mut sink) {
            Some(next) => {
                self.pos = next;
                Ok(Some(self.row.len()))
            }
            None => Ok(None),
        }
    }

    fn read_batch_into(&mut self, n: usize) -> Result<usize, String> {
        self.batch_vals.clear();
        self.batch_lens.clear();
        let mut rows = 0usize;
        for _ in 0..n {
            match self.read_one_row()? {
                Some(count) => {
                    self.batch_vals.extend(self.row.drain(..));
                    self.batch_lens.push(count);
                    rows += 1;
                }
                None => break,
            }
        }
        Ok(rows)
    }
}

#[pymethods]
impl CrxmlReader {
    #[new]
    fn new(path: String, row_tag: Option<String>) -> PyResult<Self> {
        let p = Path::new(&path);
        if !p.is_file() {
            return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
        }
        let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();
        let mmap = auto_mmap(p, false);
        let input = rypipe_core::InputBuffer::open(p, mmap, false)
            .map_err(|e| PyIOError::new_err(e.to_string()))?;
        let regions = {
            let bytes = input.as_slice();
            let (regs, _) = crate::xml::splitter::find_special_regions(bytes);
            regs
        };
        {
            let bytes = input.as_slice();
            rypipe_core::RecordParser::validate(
                &crate::xml::CrystalXmlDecoder::with_row_tag(&row_tag),
                bytes,
            )
            .map_err(|e| XmlError::new_err(e.to_string()))?;
        }
        Ok(CrxmlReader {
            parser: RowParser {
                input,
                pos: 0,
                regions,
                row_tag,
                row: Vec::with_capacity(16),
                batch_vals: Vec::with_capacity(16 * 1024),
                batch_lens: Vec::with_capacity(1024),
                #[cfg(feature = "profile")]
                profile: ProfileCounters::default(),
            },
            key_cache: rustc_hash::FxHashMap::default(),
        })
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn next_row(&mut self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        match self.parser.read_one_row().map_err(XmlError::new_err)? {
            None => Ok(None),
            Some(_n) => {
                #[cfg(feature = "profile")]
                let _dict_start = Instant::now();
                let CrxmlReader { parser, key_cache } = self;
                let dict = new_dict(py);
                for (k, v) in parser.row.drain(..) {
                    dict.set_item(cached_key(py, key_cache, &k), v)?;
                }
                #[cfg(feature = "profile")]
                {
                    self.parser.profile.dict_build_ns += _dict_start.elapsed().as_nanos() as u64;
                }
                Ok(Some(dict.into()))
            }
        }
    }

    fn __next__(mut slf: PyRefMut<'_, Self>) -> PyResult<Option<PyObject>> {
        let py = slf.py();
        slf.next_row(py)
    }

    /// Parse a batch of rows with the GIL released, then build Python dicts.
    #[pyo3(signature = (n=1024))]
    fn next_batch(mut slf: PyRefMut<'_, Self>, n: usize) -> PyResult<Option<PyObject>> {
        let py = slf.py();

        // Parse into flat buffers with GIL released. Only the pure-Rust
        // RowParser crosses into the closure; key_cache (Py objects) stays out.
        let parser: &mut RowParser = &mut slf.parser;
        let rows = py
            .allow_threads(move || parser.read_batch_into(n))
            .map_err(XmlError::new_err)?;

        if rows == 0 {
            return Ok(None);
        }

        // GIL held: build dicts from flat buffers with interned keys.
        #[cfg(feature = "profile")]
        let _dict_start = Instant::now();
        let out = PyList::empty(py);
        let CrxmlReader { parser, key_cache } = &mut *slf;
        let mut cursor = 0usize;
        for &len in &parser.batch_lens {
            let dict = new_dict(py);
            for (k, v) in &parser.batch_vals[cursor..cursor + len] {
                dict.set_item(cached_key(py, key_cache, k), v.as_str())?;
            }
            cursor += len;
            out.append(dict)?;
        }
        #[cfg(feature = "profile")]
        {
            slf.parser.profile.dict_build_ns += _dict_start.elapsed().as_nanos() as u64;
        }
        Ok(Some(out.into_any().unbind()))
    }

    #[cfg(feature = "profile")]
    fn get_profile_data(&self, py: Python<'_>) -> PyResult<PyObject> {
        let d = PyDict::new(py);
        d.set_item("event_loop_ns", self.parser.profile.event_loop_ns)?;
        d.set_item("unescape_ns", self.parser.profile.unescape_ns)?;
        d.set_item("dict_build_ns", self.parser.profile.dict_build_ns)?;
        Ok(d.into())
    }

    #[cfg(feature = "profile")]
    fn reset_profile(&mut self) {
        self.parser.profile = ProfileCounters::default();
    }
}

#[pymodule]
fn _crxml_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("XmlError", m.py().get_type::<XmlError>())?;
    m.add("PlanError", m.py().get_type::<PlanError>())?;
    m.add("MergeError", m.py().get_type::<MergeError>())?;
    m.add_class::<CrxmlReader>()?;
    m.add_function(wrap_pyfunction!(read_to_columnar, m)?)?;
    m.add_function(wrap_pyfunction!(read_to_columnar_multi, m)?)?;
    m.add_function(wrap_pyfunction!(read_to_columnar_par, m)?)?;
    m.add_function(wrap_pyfunction!(read_to_columnar_bounded, m)?)?;
    #[cfg(feature = "profile")]
    {
        m.add_function(wrap_pyfunction!(get_par_profile, m)?)?;
    }
    #[cfg(feature = "testing")]
    {
        m.add_function(wrap_pyfunction!(_test_parse_both, m)?)?;
        m.add_function(wrap_pyfunction!(_test_parse_fast, m)?)?;
        m.add_function(wrap_pyfunction!(_test_parse_quickxml, m)?)?;
    }
    Ok(())
}
