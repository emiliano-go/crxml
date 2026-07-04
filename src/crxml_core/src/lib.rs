#![deny(unsafe_code)]

use pyo3::exceptions::{PyIOError, PyException};
use pyo3::prelude::*;
use pyo3::wrap_pyfunction;
use pyo3::types::{PyDict, PyList};
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
    let chunks = splitter::compute_splits(bytes, row_tag, num_chunks);

    use rayon::prelude::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

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

    let mut merged = columnar::ColumnarEngine::new();
    for result in results {
        let engine = result.map_err(|e| PyException::new_err(e))?;
        merged.extend(engine).map_err(|e| PyException::new_err(e))?;
    }
    merged.auto_dict_upgrade();
    Python::with_gil(|py| merged.to_pyarrow_table(py))
}

/// Parse a file in bounded batches to stay within a memory budget.
/// `budget_bytes` is the approximate upper bound for intermediate
/// builder storage.  Each batch is parsed independently and exported
/// to a pyarrow table, then all batch tables are concatenated.
#[cfg(feature = "columnar")]
fn parse_columnar_bounded(
    path: &str,
    row_tag: &[u8],
    plan: columnar::BuildPlan,
    budget_bytes: usize,
) -> PyResult<PyObject> {
    use std::fs::File;
    use std::io::Read;

    let p = std::path::Path::new(path);
    let mut file = File::open(p)
        .map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| PyIOError::new_err(format!("Read error: {}", e)))?;

    if bytes.is_empty() {
        return Python::with_gil(|py| {
            let pa = PyModule::import(py, "pyarrow")?;
            Ok(pa.call_method1("table", (PyDict::new(py),))?.into())
        });
    }

    // Estimate bytes per row from the Row tag density in first 64KB
    let sample_end = bytes.len().min(65536);
    let row_tag_count = memchr::memmem::find_iter(&bytes[..sample_end], row_tag).count();
    let bytes_per_row = if row_tag_count > 0 {
        sample_end / row_tag_count
    } else {
        // Fallback: estimate from first Row tag position
        memchr::memmem::find(&bytes[..sample_end], row_tag)
            .map(|pos| pos + row_tag.len())
            .unwrap_or(512)
    }
    .max(1);

    let total_rows_est = bytes.len() / bytes_per_row;
    let rows_per_batch = (budget_bytes / bytes_per_row).max(1).min(total_rows_est.max(1));

    let num_batches = (total_rows_est / rows_per_batch).max(1);
    let chunks = splitter::compute_splits(&bytes, row_tag, num_batches.min(64));

    let mut batch_tables: Vec<PyObject> = Vec::new();
    let mut batch_engine = columnar::ColumnarEngine::with_plan(bytes_per_row.max(64), plan.clone());
    let mut rows_in_batch = 0usize;

    for chunk in &chunks {
        let mut chunk_engine = columnar::ColumnarEngine::with_plan(
            (chunk.len() / 512).max(64),
            plan.clone(),
        );
        chunk_engine
            .parse_bytes(&bytes[chunk.clone()], row_tag)
            .map_err(|e| {
                PyException::new_err(format!("Parse error in batch: {}", e))
            })?;
        let chunk_rows = chunk_engine.num_rows();
        batch_engine.extend(chunk_engine).map_err(|e| PyException::new_err(e))?;
        rows_in_batch += chunk_rows;

        if rows_in_batch >= rows_per_batch {
            batch_engine.auto_dict_upgrade();
            let table = Python::with_gil(|py| batch_engine.to_pyarrow_table(py))?;
            batch_tables.push(table);
            batch_engine.reset();
            rows_in_batch = 0;
        }
    }

    if batch_engine.num_rows() > 0 {
        batch_engine.auto_dict_upgrade();
        let table = Python::with_gil(|py| batch_engine.to_pyarrow_table(py))?;
        batch_tables.push(table);
    }

    if batch_tables.is_empty() {
        return Python::with_gil(|py| {
            let pa = PyModule::import(py, "pyarrow")?;
            Ok(pa.call_method1("table", (PyDict::new(py),))?.into())
        });
    }

    if batch_tables.len() == 1 {
        return Ok(batch_tables.into_iter().next().unwrap());
    }

    Python::with_gil(|py| {
        let pa = PyModule::import(py, "pyarrow")?;
        let tables_list = PyList::new(py, &batch_tables)?;
        let concat = pa.call_method1("concat_tables", (tables_list,))?;
        Ok(concat.into())
    })
}

#[cfg(feature = "mmap")]
fn mmap_and_parse(
    path: &str,
    row_tag: &[u8],
    plan: columnar::BuildPlan,
) -> PyResult<PyObject> {
    let p = Path::new(path);
    let file = File::open(p)
        .map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;
    // SAFETY: The caller must not truncate or write to the file while the
    // mapping exists. crxml is a read-only parser; no writes occur.
    #[allow(unsafe_code)]
    let mmap = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|e| PyIOError::new_err(format!("Cannot mmap {}: {}", path, e)))?;
    let _ = mmap.advise(memmap2::Advice::Sequential);
    let _ = mmap.advise(memmap2::Advice::WillNeed);
    parse_columnar_from_slice(&mmap[..], row_tag, plan)
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
        plan.field_map = map;
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
#[pyo3(signature = (path, row_tag=None, field_mapping=None, drop_fields=None, filter=None, field_types=None, dictionary_columns=None, use_mmap=false, schema=None, auto_dict=false))]
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
            return mmap_and_parse(&path, &row_tag, plan);
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
#[pyo3(signature = (path, row_tag=None, num_chunks=2, field_mapping=None, drop_fields=None, filter=None, field_types=None, dictionary_columns=None, use_mmap=false, schema=None, auto_dict=false))]
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
            let mmap_path = path.as_str();
            let p_file = Path::new(mmap_path);
            let file = File::open(p_file).map_err(|e| {
                PyIOError::new_err(format!("Cannot open {}: {}", mmap_path, e))
            })?;
            #[allow(unsafe_code)]
            let mmap = unsafe { memmap2::Mmap::map(&file) }
                .map_err(|e| PyIOError::new_err(format!("Cannot mmap {}: {}", mmap_path, e)))?;
            let _ = mmap.advise(memmap2::Advice::Sequential);
            let _ = mmap.advise(memmap2::Advice::WillNeed);
            return parse_columnar_multi_from_slice(&mmap[..], &row_tag, plan, num_chunks);
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
#[pyo3(signature = (path, row_tag=None, num_chunks=4, field_mapping=None, drop_fields=None, filter=None, field_types=None, dictionary_columns=None, use_mmap=false, schema=None, auto_dict=false))]
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
            let mmap_path = path.as_str();
            let p_file = Path::new(mmap_path);
            let file = File::open(p_file).map_err(|e| {
                PyIOError::new_err(format!("Cannot open {}: {}", mmap_path, e))
            })?;
            #[allow(unsafe_code)]
            let mmap = unsafe { memmap2::Mmap::map(&file) }
                .map_err(|e| PyIOError::new_err(format!("Cannot mmap {}: {}", mmap_path, e)))?;
            let _ = mmap.advise(memmap2::Advice::Sequential);
            let _ = mmap.advise(memmap2::Advice::WillNeed);
            return parse_columnar_par_from_slice(&mmap[..], &row_tag, plan, num_chunks);
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
) -> PyResult<PyObject> {
    let plan = build_plan_from_kwargs(
        field_mapping, drop_fields, filter, field_types, dictionary_columns,
        schema, auto_dict,
    )?;
    let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();
    parse_columnar_bounded(&path, &row_tag, plan, memory)
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

/// Streaming CR XML row parser.
///
/// # Load-bearing invariants
/// - Holds **no** `Py<...>` objects. This is required for `py.allow_threads` to
///   compile (the `Send` bound on the closure). If key-interning is ever revived
///   it must live separately, not on this struct.
#[pyclass]
pub struct CrxmlReader {
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

// Pure-Rust helpers (not #[pymethods]) — no Python objects touched.
impl CrxmlReader {
    fn read_one_row(&mut self) -> Result<Option<usize>, String> {
        #[cfg(feature = "profile")]
        let profile = &mut self.profile;
        let CrxmlReader { reader, buf, inner_buf, row, row_tag, .. } = self;
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
            reader,
            buf: Vec::with_capacity(4096),
            inner_buf: Vec::with_capacity(4096),
            row: Vec::with_capacity(16),
            row_tag,
            batch_vals: Vec::with_capacity(16 * 1024),
            batch_lens: Vec::with_capacity(1024),
            #[cfg(feature = "profile")]
            profile: ProfileCounters::default(),
        })
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn next_row(&mut self, py: Python<'_>) -> PyResult<Option<PyObject>> {
        match self.read_one_row().map_err(|e| PyException::new_err(e))? {
            None => Ok(None),
            Some(_) => {
                #[cfg(feature = "profile")]
                let _dict_start = Instant::now();
                let dict = PyDict::new(py);
                for (k, v) in self.row.drain(..) {
                    dict.set_item(k, v)?;
                }
                #[cfg(feature = "profile")]
                {
                    self.profile.dict_build_ns += _dict_start.elapsed().as_nanos() as u64;
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

        // Parse into flat buffers with GIL released.
        let this: &mut CrxmlReader = &mut *slf;
        let rows = py
            .allow_threads(move || this.read_batch_into(n))
            .map_err(PyException::new_err)?;

        if rows == 0 {
            return Ok(None);
        }

        // GIL held: build dicts from flat buffers.
        #[cfg(feature = "profile")]
        let _dict_start = Instant::now();
        let out = PyList::empty(py);
        let mut cursor = 0usize;
        for &len in &slf.batch_lens {
            let dict = PyDict::new(py);
            for (k, v) in &slf.batch_vals[cursor..cursor + len] {
                dict.set_item(k.as_str(), v.as_str())?;
            }
            cursor += len;
            out.append(dict)?;
        }
        #[cfg(feature = "profile")]
        {
            slf.profile.dict_build_ns += _dict_start.elapsed().as_nanos() as u64;
        }
        Ok(Some(out.into_any().unbind()))
    }

    #[cfg(feature = "profile")]
    fn get_profile_data(&self, py: Python<'_>) -> PyResult<PyObject> {
        let d = PyDict::new(py);
        d.set_item("event_loop_ns", self.profile.event_loop_ns)?;
        d.set_item("unescape_ns", self.profile.unescape_ns)?;
        d.set_item("dict_build_ns", self.profile.dict_build_ns)?;
        Ok(d.into())
    }

    #[cfg(feature = "profile")]
    fn reset_profile(&mut self) {
        self.profile = ProfileCounters::default();
    }

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
    }
    Ok(())
}
