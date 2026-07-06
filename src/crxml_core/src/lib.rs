#![deny(unsafe_code)]

use pyo3::exceptions::{PyIOError, PyException};
use pyo3::prelude::*;
use pyo3::wrap_pyfunction;
use pyo3::types::{PyDict, PyList, PyString};
use quick_xml::events::Event;
use quick_xml::Reader;
use std::fs::File;
use std::io::BufReader;
#[cfg(feature = "columnar")]
use std::io::Read;
use std::path::Path;
#[cfg(feature = "columnar")]
use std::collections::HashMap;
#[cfg(feature = "profile")]
use std::time::Instant;

// Fast allocator: replaces the system heap for all Rust-side
// allocations (profiling showed ~27% of CPU in malloc/free).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "profile")]
use std::sync::Mutex;
#[cfg(feature = "profile")]
static PAR_PROFILE: Mutex<ParProfileSnapshot> = Mutex::new(ParProfileSnapshot {
    split_scan_ns: 0,
    parse_ns: 0,
    assembly_export_ns: 0,
});

#[cfg(feature = "profile")]
#[derive(Default, Clone)]
struct ParProfileSnapshot {
    pub split_scan_ns: u64,
    pub parse_ns: u64,
    pub assembly_export_ns: u64,
}

#[cfg(feature = "columnar")]
pub mod columnar;
#[cfg(feature = "columnar")]
pub mod splitter;

#[cfg(feature = "columnar")]
fn parse_columnar_from_slice(
    bytes: &[u8],
    row_tag: &[u8],
    plan: columnar::BuildPlan,
) -> PyResult<PyObject> {
    let mut engine =
        columnar::ColumnarEngine::with_plan((bytes.len() / 512).max(64), plan);
    engine
        .parse_bytes(bytes, row_tag)
        .map_err(|e| PyException::new_err(format!("Columnar parse error: {}", e)))?;
    engine.auto_dict_upgrade();
    Python::with_gil(|py| engine.to_pyarrow_table(py))
}

#[cfg(feature = "columnar")]
fn parse_columnar_multi_from_slice(
    bytes: &[u8],
    row_tag: &[u8],
    plan: columnar::BuildPlan,
    num_chunks: usize,
) -> PyResult<PyObject> {
    let chunks = splitter::compute_splits(bytes, row_tag, num_chunks);
    let mut merged = columnar::ColumnarEngine::new();
    for chunk in &chunks {
        let mut engine = columnar::ColumnarEngine::with_plan(
            (chunk.len() / 512).max(64),
            plan.clone(),
        );
        engine
            .parse_bytes(&bytes[chunk.clone()], row_tag)
            .map_err(|e| {
                PyException::new_err(format!(
                    "Columnar parse error in chunk {:?}: {}",
                    chunk, e
                ))
            })?;
        merged.extend(engine).map_err(|e| PyException::new_err(e))?;
    }
    merged.auto_dict_upgrade();
    Python::with_gil(|py| merged.to_pyarrow_table(py))
}

#[cfg(feature = "columnar")]
fn parse_columnar_par_from_slice(
    bytes: &[u8],
    row_tag: &[u8],
    plan: columnar::BuildPlan,
    num_chunks: usize,
) -> PyResult<PyObject> {
    #[cfg(feature = "profile")]
    let split_start = Instant::now();
    let chunks = splitter::compute_splits(bytes, row_tag, num_chunks);
    #[cfg(feature = "profile")]
    let split_scan_ns = split_start.elapsed().as_nanos() as u64;

    use rayon::prelude::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[cfg(feature = "profile")]
    let parse_start = Instant::now();
    let results: Vec<Result<columnar::ColumnarEngine, String>> = chunks
        .par_iter()
        .map(|range| {
            let range = range.clone();
            let plan = plan.clone();
            catch_unwind(AssertUnwindSafe(move || {
                let est = if range.len() > 0 {
                    (range.len() / 512).max(64)
                } else {
                    64
                };
                let mut engine = columnar::ColumnarEngine::with_plan(est, plan);
                engine
                    .parse_bytes(&bytes[range.clone()], row_tag)
                    .map_err(|e| format!("Parse error in chunk {:?}: {}", range, e))?;
                Ok(engine)
            }))
            .unwrap_or_else(|_| Err("Worker panicked during parallel parse".to_string()))
        })
        .collect();
    #[cfg(feature = "profile")]
    let parse_ns = parse_start.elapsed().as_nanos() as u64;

    // Fast path: export chunks as record batches in parallel, no merge.
    if !plan.auto_dict {
        let engines = results
            .into_iter()
            .collect::<Result<Vec<_>, String>>()
            .map_err(PyException::new_err)?;
        #[cfg(feature = "profile")]
        let gil_start = Instant::now();
        let result = Python::with_gil(|py| columnar::engines_to_pyarrow_table(engines, &plan, py));
        #[cfg(feature = "profile")]
        {
            if let Ok(mut p) = PAR_PROFILE.lock() {
                p.split_scan_ns = split_scan_ns;
                p.parse_ns = parse_ns;
                p.assembly_export_ns = gil_start.elapsed().as_nanos() as u64;
            }
        }
        return result;
    }

    // auto_dict path: incremental merge — fold each chunk engine into the
    // accumulator one at a time, dropping the chunk engine after extend.
    // This bounds peak RSS to merged-so-far (~1x) plus one chunk (~1x/N)
    // instead of all chunks at once (~5x).
    #[cfg(feature = "profile")]
    let gil_start = Instant::now();
    let mut merged =
        columnar::ColumnarEngine::with_plan(results.len().max(64) * 512, plan);
    for result in results {
        let engine = result.map_err(PyException::new_err)?;
        merged.extend(engine).map_err(|e| PyException::new_err(e))?;
    }
    merged.auto_dict_upgrade();
    let result = Python::with_gil(|py| merged.to_pyarrow_table(py));
    #[cfg(feature = "profile")]
    {
        if let Ok(mut p) = PAR_PROFILE.lock() {
            p.split_scan_ns = split_scan_ns;
            p.parse_ns = parse_ns;
            p.assembly_export_ns = gil_start.elapsed().as_nanos() as u64;
        }
    }
    result
}

/// Parse a file in bounded batches to stay within a memory budget.
/// `budget_bytes` is the approximate upper bound for intermediate
/// builder storage.  Each batch is parsed independently and exported
/// to a pyarrow table, then all batch tables are concatenated.
#[cfg(feature = "columnar")]
fn concat_tables(a: PyObject, b: PyObject) -> PyResult<PyObject> {
    Python::with_gil(|py| {
        let pa = PyModule::import(py, "pyarrow")?;
        let tables_list = PyList::new(py, vec![a, b])?;

        // Fast path: schemas match — no promotion needed.
        let a_schema = tables_list.get_item(0)?.getattr("schema")?;
        let b_schema = tables_list.get_item(1)?.getattr("schema")?;
        let schemas_match = a_schema.call_method1("__eq__", (b_schema,))?.is_truthy()?;

        if schemas_match {
            return Ok(pa.call_method1("concat_tables", (tables_list,))?.into());
        }

        // Schemas differ (column order or auto_dict promotion): use promote.
        let kwargs = PyDict::new(py);
        kwargs.set_item("promote_options", "default")?;
        Ok(pa.call_method("concat_tables", (tables_list,), Some(&kwargs))?.into())
    })
}

fn parse_columnar_bounded(
    path: &str,
    row_tag: &[u8],
    plan: columnar::BuildPlan,
    budget_bytes: usize,
    prefault: bool,
) -> PyResult<PyObject> {
    let p = std::path::Path::new(path);
    let mmap_handle = MmapHandle::new(p, prefault)?;
    let bytes = mmap_handle.as_slice();
    let file_len = bytes.len();

    if file_len == 0 {
        return Python::with_gil(|py| {
            let pa = PyModule::import(py, "pyarrow")?;
            Ok(pa.call_method1("table", (PyDict::new(py),))?.into())
        });
    }

    // Estimate bytes per row from the Row tag density in first 64KB
    let sample_end = file_len.min(65536);
    let row_tag_count = memchr::memmem::find_iter(&bytes[..sample_end], row_tag).count();
    let bytes_per_row = if row_tag_count > 0 {
        sample_end / row_tag_count
    } else {
        memchr::memmem::find(&bytes[..sample_end], row_tag)
            .map(|pos| pos + row_tag.len())
            .unwrap_or(512)
    }
    .max(1);

    let total_rows_est = file_len / bytes_per_row;
    let rows_per_batch = (budget_bytes / bytes_per_row).max(1).min(total_rows_est.max(1));

    let num_batches = (total_rows_est / rows_per_batch).max(1);
    let chunks = splitter::compute_splits(bytes, row_tag, num_batches.min(64));

    // Explicitly end the mmap borrow before creating Python objects
    drop(mmap_handle);

    // ---- processing phase (mmap is gone, no Python refs to mmaped memory) ----
    let mut result_table: Option<PyObject> = None;
    let mut batch_engine = columnar::ColumnarEngine::with_plan(bytes_per_row.max(64), plan.clone());
    let mut rows_in_batch = 0usize;

    // Re-read the file chunk by chunk, seeking to each range
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(p)
        .map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;

    for chunk in &chunks {
        let chunk_len = chunk.len();
        let mut chunk_buf = vec![0u8; chunk_len];
        file.seek(SeekFrom::Start(chunk.start as u64))
            .map_err(|e| PyIOError::new_err(format!("Seek error: {}", e)))?;
        file.read_exact(&mut chunk_buf)
            .map_err(|e| PyIOError::new_err(format!("Read error: {}", e)))?;

        let mut chunk_engine = columnar::ColumnarEngine::with_plan(
            (chunk_len / 512).max(64),
            plan.clone(),
        );
        chunk_engine
            .parse_bytes(&chunk_buf, row_tag)
            .map_err(|e| {
                PyException::new_err(format!("Parse error in batch: {}", e))
            })?;
        // chunk_buf dropped here

        let chunk_rows = chunk_engine.num_rows();
        batch_engine.extend(chunk_engine).map_err(|e| PyException::new_err(e))?;
        rows_in_batch += chunk_rows;

        if rows_in_batch >= rows_per_batch {
            batch_engine.auto_dict_upgrade();
            batch_engine.sort_columns();
            let table = Python::with_gil(|py| batch_engine.to_pyarrow_table(py))?;
            result_table = match result_table {
                None => Some(table),
                Some(prev) => Some(concat_tables(prev, table)?),
            };
            batch_engine.reset();
            rows_in_batch = 0;
        }
    }

    if batch_engine.num_rows() > 0 {
        batch_engine.auto_dict_upgrade();
        batch_engine.sort_columns();
        let table = Python::with_gil(|py| batch_engine.to_pyarrow_table(py))?;
        result_table = match result_table {
            None => Some(table),
            Some(prev) => Some(concat_tables(prev, table)?),
        };
    }

    match result_table {
        None => Python::with_gil(|py| {
            let pa = PyModule::import(py, "pyarrow")?;
            Ok(pa.call_method1("table", (PyDict::new(py),))?.into())
        }),
        Some(t) => Ok(t),
    }
}

/// Owned mmap with explicit lifecycle: parse phase ends before unmap.
///
/// # Safety invariant
///
/// All data read through `as_slice()` is **copied** into owned storage
/// (`ColumnBuilder::push_str` → `StrColumn::push` → `extend_from_slice`,
/// typed columns parse and discard the string). No borrowed reference to
/// the mapped bytes survives return from `to_pyarrow_table`. The Arrow C
/// Data Interface export creates fresh `Arc<[u8]>` buffers via
/// `Buffer::from_slice_ref`. Therefore the `Mmap` can be safely dropped
/// (via `unmap_now`) after export completes.
///
/// If a future change introduces zero-copy Arrow arrays over the mmap
/// slice, this invariant is broken — the unmap must remain synchronous.
#[cfg(feature = "mmap")]
struct MmapHandle {
    mmap: memmap2::Mmap,
}

#[cfg(feature = "mmap")]
impl MmapHandle {
    fn new(path: &Path, prefault: bool) -> PyResult<Self> {
        let file = File::open(path)
            .map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path.display(), e)))?;
        #[allow(unsafe_code)]
        let mmap = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| PyIOError::new_err(format!("Cannot mmap {}: {}", path.display(), e)))?;
        #[cfg(unix)]
        {
            if prefault {
                // WillNeed: pre-fault the entire file into RSS.  Use when
                // the goal is parse speed (DataFrame, file fits in RAM) and
                // the RSS cost is acceptable.
                let _ = mmap.advise(memmap2::Advice::WillNeed);
            } else {
                // Sequential: kernel drops behind pages we have already read.
                // Use when RSS matters (bounded path, large files).
                let _ = mmap.advise(memmap2::Advice::Sequential);
            }
        }
        Ok(MmapHandle { mmap })
    }

    /// Borrow the mapped bytes for parsing. The borrow must end before
    /// `unmap_now` is called — the Rust borrow checker enforces this.
    fn as_slice(&self) -> &[u8] {
        &self.mmap[..]
    }

    /// Consume the handle and drop the mapping synchronously.
    /// Must be called *after* the parse result is fully materialized and
    /// the borrow from `as_slice()` has ended.
    /// Synchronous unmap ensures file-backed pages are released from the
    /// process address space before any downstream work (Arrow export,
    /// pandas conversion) begins, keeping peak RSS lower.
    fn unmap_now(self) {
        drop(self.mmap);
    }
}

#[cfg(feature = "mmap")]
fn mmap_and_parse(
    path: &str,
    row_tag: &[u8],
    plan: columnar::BuildPlan,
    prefault: bool,
) -> PyResult<PyObject> {
    let p = Path::new(path);
    let handle = MmapHandle::new(p, prefault)?;
    let result = parse_columnar_from_slice(handle.as_slice(), row_tag, plan);
    // parse_columnar_from_slice copies all data into owned Vec<u8> /
    // Arrow buffers before returning. The borrow of handle.as_slice()
    // has ended — it is now safe to reclaim the mapping.
    handle.unmap_now();
    result
}

#[cfg(feature = "mmap")]
fn mmap_and_parse_multi(
    path: &str,
    row_tag: &[u8],
    plan: columnar::BuildPlan,
    num_chunks: usize,
    prefault: bool,
) -> PyResult<PyObject> {
    let p = Path::new(path);
    let handle = MmapHandle::new(p, prefault)?;
    let result = parse_columnar_multi_from_slice(handle.as_slice(), row_tag, plan, num_chunks);
    handle.unmap_now();
    result
}

#[cfg(feature = "mmap")]
fn mmap_and_parse_par(
    path: &str,
    row_tag: &[u8],
    plan: columnar::BuildPlan,
    num_chunks: usize,
    prefault: bool,
) -> PyResult<PyObject> {
    let p = Path::new(path);
    let handle = MmapHandle::new(p, prefault)?;
    let result = parse_columnar_par_from_slice(handle.as_slice(), row_tag, plan, num_chunks);
    handle.unmap_now();
    result
}

#[cfg(feature = "columnar")]
fn build_plan_from_kwargs(
    field_mapping: Option<HashMap<String, String>>,
    drop_fields: Option<Vec<String>>,
    filter: Option<HashMap<String, String>>,
    field_types: Option<HashMap<String, String>>,
    dictionary_columns: Option<Vec<String>>,
    schema: Option<Vec<String>>,
    auto_dict: bool,
) -> PyResult<columnar::BuildPlan> {
    let mut plan = columnar::BuildPlan::new();

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
            let ft = columnar::FieldType::from_str(&type_str)
                .ok_or_else(|| {
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
            .ok_or_else(|| PyException::new_err("filter must include 'op' key"))?
            .to_owned();
        // Column-to-column filter: field_a + op + field_b
        if f.contains_key("field_a") && f.contains_key("field_b") {
            let field_a = f.get("field_a").unwrap().to_owned();
            let field_b = f.get("field_b").unwrap().to_owned();
            let cop = columnar::CompareOp::from_str(&op).ok_or_else(|| {
                let valid = ">, <, >=, <=, ==, !=";
                PyException::new_err(format!(
                    "unsupported compare op {op:?}; valid: {valid}"
                ))
            })?;
            plan.filter = Some(columnar::FilterPredicate::Compare {
                field_a,
                op: cop,
                field_b,
            });
        } else {
            let field = f
                .get("field")
                .ok_or_else(|| PyException::new_err("filter must include 'field' key"))?
                .to_owned();
            let value = f
                .get("value")
                .ok_or_else(|| PyException::new_err("filter must include 'value' key"))?
                .to_owned();
            plan.filter = Some(match op.as_str() {
                "!=" | "ne" => columnar::FilterPredicate::NotEqual { field, value },
                "==" | "eq" => columnar::FilterPredicate::Equal { field, value },
                other => {
                    let msg = format!("unsupported filter op {other:?}; use '!=' or '=='");
                    return Err(PyException::new_err(msg));
                }
            });
        }
    }

    Ok(plan)
}

#[cfg(feature = "columnar")]
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
        field_mapping, drop_fields, filter, field_types, dictionary_columns, schema, auto_dict,
    )?;

    let p = Path::new(&path);
    if !p.is_file() {
        return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
    }
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();

    if use_mmap {
        #[cfg(feature = "mmap")]
        {
            return mmap_and_parse(&path, &row_tag, plan, prefault);
        }
        #[cfg(not(feature = "mmap"))]
        {
            return Err(PyException::new_err(
                "mmap requires the 'mmap' Cargo feature. Rebuild with --features=mmap",
            ));
        }
    }

    let mut file =
        File::open(p).map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| PyIOError::new_err(format!("Read error: {}", e)))?;

    parse_columnar_from_slice(&bytes, &row_tag, plan)
}

#[cfg(feature = "columnar")]
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
        field_mapping, drop_fields, filter, field_types, dictionary_columns, schema, auto_dict,
    )?;

    let p = Path::new(&path);
    if !p.is_file() {
        return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
    }
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();

    if use_mmap {
        #[cfg(feature = "mmap")]
        {
            return mmap_and_parse_multi(&path, &row_tag, plan, num_chunks, prefault);
        }
        #[cfg(not(feature = "mmap"))]
        {
            return Err(PyException::new_err(
                "mmap requires the 'mmap' Cargo feature. Rebuild with --features=mmap",
            ));
        }
    }

    let mut file =
        File::open(p).map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| PyIOError::new_err(format!("Read error: {}", e)))?;

    parse_columnar_multi_from_slice(&bytes, &row_tag, plan, num_chunks)
}

#[cfg(feature = "columnar")]
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
        field_mapping, drop_fields, filter, field_types, dictionary_columns, schema, auto_dict,
    )?;

    let p = Path::new(&path);
    if !p.is_file() {
        return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
    }
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();

    if use_mmap {
        #[cfg(feature = "mmap")]
        {
            return mmap_and_parse_par(&path, &row_tag, plan, num_chunks, prefault);
        }
        #[cfg(not(feature = "mmap"))]
        {
            return Err(PyException::new_err(
                "mmap requires the 'mmap' Cargo feature. Rebuild with --features=mmap",
            ));
        }
    }

    let mut file =
        File::open(p).map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| PyIOError::new_err(format!("Read error: {}", e)))?;

    parse_columnar_par_from_slice(&bytes, &row_tag, plan, num_chunks)
}

#[cfg(feature = "columnar")]
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
        field_mapping, drop_fields, filter, field_types, dictionary_columns,
        schema, auto_dict,
    )?;
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();
    parse_columnar_bounded(&path, &row_tag, plan, memory, prefault)
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
    ($profile:expr, $field:ident, $body:expr) => { $body };
}

/// Pure-Rust parsing state for the stream engine.
///
/// # Load-bearing invariants
/// - Holds **no** `Py<...>` objects. This is required for `py.allow_threads`
///   to compile (the `Ungil` bound on the closure). Python-object state (the
///   interned-key cache) lives on `CrxmlReader` beside this struct, and only
///   the GIL-held dict-build code touches it.
struct RowParser {
    reader: Reader<BufReader<File>>,
    buf: Vec<u8>,
    inner_buf: Vec<u8>,
    /// Per-row scratch buffer (cleared each row, retains capacity).
    row: Vec<(String, String)>,
    row_tag: Vec<u8>,
    /// Flat field buffer for batched output. Grows to fit one batch.
    batch_vals: Vec<(String, String)>,
    /// Field count per row in `batch_vals`, for slicing.
    batch_lens: Vec<usize>,
    #[cfg(feature = "profile")]
    profile: ProfileCounters,
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

// Pure-Rust helpers (not #[pymethods]) — no Python objects touched.
impl RowParser {
    fn read_one_row(&mut self) -> Result<Option<usize>, String> {
        #[cfg(feature = "profile")]
        let profile = &mut self.profile;
        let RowParser { reader, buf, inner_buf, row, row_tag, .. } = self;
        row.clear();

        loop {
            let event = measure_el!(profile, event_loop_ns, reader
                .read_event_into(buf)
                .map_err(|e| format!("XML parse error: {}", e))?);

            match event {
                Event::Empty(ref e) if e.name().as_ref() == row_tag.as_slice() => {
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| format!("Attribute error: {}", e))?;
                        let key = std::str::from_utf8(attr.key.as_ref())
                            .map_err(|e| format!("Non-UTF8 attribute key: {}", e))?;
                        let value = measure_el!(profile, unescape_ns, attr
                            .unescape_value()
                            .map_err(|e| format!("Value unescape error: {}", e))?);
                        row.push((key.to_owned(), value.into_owned()));
                    }
                    buf.clear();
                    return Ok(Some(row.len()));
                }

                Event::Start(ref e) if e.name().as_ref() == row_tag.as_slice() => {
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| format!("Attribute error: {}", e))?;
                        let key = std::str::from_utf8(attr.key.as_ref())
                            .map_err(|e| format!("Non-UTF8 attribute key: {}", e))?;
                        let value = measure_el!(profile, unescape_ns, attr
                            .unescape_value()
                            .map_err(|e| format!("Value unescape error: {}", e))?);
                        row.push((key.to_owned(), value.into_owned()));
                    }

                    loop {
                        let child_event = measure_el!(profile, event_loop_ns, reader
                            .read_event_into(buf)
                            .map_err(|e| format!("XML parse error: {}", e))?);

                        match child_event {
                            Event::Start(ref child) | Event::Empty(ref child) => {
                                let child_name = child.name();
                                let child_tag = child_name.as_ref();

                                if child_tag == b"Field" {
                                    let mut field_name: Option<String> = None;
                                    for attr in child.attributes() {
                                        if let Ok(attr) = attr {
                                            let attr_key = attr.key.as_ref();
                                            if attr_key == b"FieldName" || attr_key == b"Name" {
                                                if let Ok(value) = measure_el!(profile, unescape_ns, attr.unescape_value()) {
                                                    field_name = Some(value.into_owned());
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    let key = field_name.unwrap_or_else(|| "Field".to_string());

                                    let mut text = String::new();
                                    if matches!(child_event, Event::Start(_)) {
                                        let field_end_bytes = child_name.as_ref();
                                        loop {
                                            let inner = measure_el!(profile, event_loop_ns, reader
                                                .read_event_into(inner_buf)
                                                .map_err(|e| format!("XML parse error: {}", e))?);
                                            match inner {
                                                Event::Start(ref inner_child)
                                                | Event::Empty(ref inner_child) => {
                                                    let inner_child_name = inner_child.name();
                                                    let inner_tag = inner_child_name.as_ref();
                                                    if inner_tag == b"FormattedValue"
                                                        || inner_tag == b"Value"
                                                    {
                                                        if matches!(inner, Event::Start(_)) {
                                                            let text_event = measure_el!(profile, event_loop_ns, reader
                                                                .read_event_into(inner_buf)
                                                                .map_err(|e| {
                                                                    format!("Text read error: {}", e)
                                                                })?);
                                                            if let Event::Text(txt) = text_event {
                                                                text = measure_el!(profile, unescape_ns, txt
                                                                    .unescape()
                                                                    .map_err(|e| {
                                                                        format!(
                                                                            "Text unescape error: {}",
                                                                            e
                                                                        )
                                                                    })?
                                                                    .into_owned());
                                                            }
                                                        }
                                                        inner_buf.clear();
                                                    }
                                                }
                                                Event::End(ref e)
                                                    if e.name().as_ref()
                                                        == field_end_bytes =>
                                                {
                                                    break;
                                                }
                                                Event::Eof => return Ok(None),
                                                _ => {}
                                            }
                                        }
                                    }
                                    row.push((key, text));
                                } else if child_tag == b"Text" {
                                    let mut text_name: Option<String> = None;
                                    for attr in child.attributes() {
                                        if let Ok(attr) = attr {
                                            if attr.key.as_ref() == b"Name" {
                                                if let Ok(value) = measure_el!(profile, unescape_ns, attr.unescape_value()) {
                                                    text_name = Some(value.into_owned());
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    let key = text_name.unwrap_or_else(|| "Text".to_string());

                                    let mut text = String::new();
                                    if matches!(child_event, Event::Start(_)) {
                                        let text_end_bytes = child_name.as_ref();
                                        loop {
                                            let inner = measure_el!(profile, event_loop_ns, reader
                                                .read_event_into(inner_buf)
                                                .map_err(|e| format!("XML parse error: {}", e))?);
                                            match inner {
                                                Event::Start(ref inner_child)
                                                | Event::Empty(ref inner_child) => {
                                                    let inner_child_name = inner_child.name();
                                                    let inner_tag = inner_child_name.as_ref();
                                                    if inner_tag == b"TextValue"
                                                    {
                                                        if matches!(inner, Event::Start(_)) {
                                                            let text_event = measure_el!(profile, event_loop_ns, reader
                                                                .read_event_into(inner_buf)
                                                                .map_err(|e| {
                                                                    format!("Text read error: {}", e)
                                                                })?);
                                                            if let Event::Text(txt) = text_event {
                                                                text = measure_el!(profile, unescape_ns, txt
                                                                    .unescape()
                                                                    .map_err(|e| {
                                                                        format!(
                                                                            "Text unescape error: {}",
                                                                            e
                                                                        )
                                                                    })?
                                                                    .into_owned());
                                                            }
                                                        }
                                                        inner_buf.clear();
                                                    }
                                                }
                                                Event::End(ref e)
                                                    if e.name().as_ref()
                                                        == text_end_bytes =>
                                                {
                                                    break;
                                                }
                                                Event::Eof => return Ok(None),
                                                _ => {}
                                            }
                                        }
                                    }
                                    row.push((key, text));
                                } else if child_tag == b"Section" {
                                    let sn = child
                                        .attributes()
                                        .filter_map(|a| a.ok())
                                        .find(|a| a.key.as_ref() == b"SectionNumber")
                                        .and_then(|a| measure_el!(profile, unescape_ns, a.unescape_value().ok()))
                                        .unwrap_or_default()
                                        .into_owned();
                                    row.push(("Section".to_string(), sn));
                                } else {
                                    let key = std::str::from_utf8(child_tag)
                                        .map_err(|e| {
                                            format!("Non-UTF8 tag name: {}", e)
                                        })?
                                        .to_owned();
                                    row.push((key, String::new()));
                                }
                            }

                            Event::End(ref e) if e.name().as_ref() == row_tag.as_slice() => {
                                break;
                            }
                            Event::Eof => return Ok(None),
                            _ => {}
                        }
                    }
                    return Ok(Some(row.len()));
                }

                Event::Eof => return Ok(None),
                _ => {}
            }
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
        let file = File::open(p)
            .map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;
        let reader = Reader::from_reader(BufReader::with_capacity(128 * 1024, file));
        let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();
        Ok(CrxmlReader {
            parser: RowParser {
                reader,
                buf: Vec::with_capacity(4096),
                inner_buf: Vec::with_capacity(4096),
                row: Vec::with_capacity(16),
                row_tag,
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
        match self.parser.read_one_row().map_err(|e| PyException::new_err(e))? {
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
            .map_err(PyException::new_err)?;

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

/// Testing helper: parse bytes with the columnar engine (quick-xml parser).
/// Retained for API compatibility with the hardening harness.
#[cfg(feature = "testing")]
#[pyfunction]
#[pyo3(signature = (bytes, row_tag=None))]
fn _test_parse_both(
    bytes: Vec<u8>,
    row_tag: Option<String>,
) -> PyResult<(PyObject, PyObject)> {
    use columnar::ColumnarEngine;
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();
    let plan = columnar::BuildPlan::new();
    let est = (bytes.len() / 512).max(64);
    let result: PyObject = Python::with_gil(|py| {
        let mut engine = ColumnarEngine::with_plan(est, plan);
        match engine.parse_bytes_quickxml_only(&bytes, &row_tag) {
            Ok(()) => {
                engine.auto_dict_upgrade();
                engine.to_pyarrow_table(py).unwrap_or_else(|_| py.None())
            }
            Err(_) => py.None(),
        }
    });
    let r2 = Python::with_gil(|py| result.clone_ref(py));
    Ok((r2, result))
}

#[cfg(feature = "testing")]
fn _run_parser(bytes: &[u8], row_tag: &[u8]) -> PyObject {
    use columnar::ColumnarEngine;
    let plan = columnar::BuildPlan::new();
    let est = (bytes.len() / 512).max(64);
    Python::with_gil(|py| {
        let mut col = ColumnarEngine::with_plan(est, plan);
        if col.parse_bytes_quickxml_only(bytes, row_tag).is_ok() {
            col.auto_dict_upgrade();
            col.to_pyarrow_table(py).unwrap_or_else(|_| py.None())
        } else {
            py.None()
        }
    })
}

/// Testing helper: parse bytes (identical to _test_parse_quickxml).
/// Kept for backward compatibility with benchmarks that reference it.
#[cfg(feature = "testing")]
#[pyfunction]
#[pyo3(signature = (bytes, row_tag=None))]
fn _test_parse_fast(bytes: Vec<u8>, row_tag: Option<String>) -> PyObject {
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();
    _run_parser(&bytes, &row_tag)
}

/// Testing helper: parse bytes with the columnar engine.
#[cfg(feature = "testing")]
#[pyfunction]
#[pyo3(signature = (bytes, row_tag=None))]
fn _test_parse_quickxml(bytes: Vec<u8>, row_tag: Option<String>) -> PyObject {
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();
    _run_parser(&bytes, &row_tag)
}

#[cfg(feature = "profile")]
#[pyfunction]
fn get_par_profile(py: Python<'_>) -> PyResult<PyObject> {
    let snap = PAR_PROFILE.lock().map_err(|e| PyException::new_err(e.to_string()))?.clone();
    let d = PyDict::new(py);
    d.set_item("split_scan_ns", snap.split_scan_ns)?;
    d.set_item("parse_ns", snap.parse_ns)?;
    d.set_item("assembly_export_ns", snap.assembly_export_ns)?;
    Ok(d.into())
}

#[pymodule]
fn _crxml_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<CrxmlReader>()?;
    #[cfg(feature = "columnar")]
    {
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
    }
    Ok(())
}
