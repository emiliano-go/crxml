#![forbid(unsafe_code)]

use pyo3::exceptions::{PyIOError, PyException};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::fs::File;
use std::io::BufReader;
#[cfg(feature = "columnar")]
use std::io::Read;
use std::path::Path;

#[cfg(feature = "columnar")]
pub mod columnar;
#[cfg(feature = "columnar")]
pub mod splitter;

#[pyclass]
pub struct CrxmlReader {
    reader: Reader<BufReader<File>>,
    buf: Vec<u8>,
    inner_buf: Vec<u8>,
    row: Vec<(String, String)>,
    row_tag: Vec<u8>,
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
        })
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(mut slf: PyRefMut<'_, Self>) -> PyResult<Option<PyObject>> {
        let py = slf.py();
        let CrxmlReader { reader, buf, inner_buf, row, row_tag } = &mut *slf;

        loop {
            let event = reader.read_event_into(buf).map_err(|e| {
                PyException::new_err(format!("XML parse error: {}", e))
            })?;

            match event {
                Event::Empty(ref e) if e.name().as_ref() == row_tag.as_slice() => {
                    let dict = PyDict::new(py);
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| PyException::new_err(format!("Attribute error: {}", e)))?;
                        let key = std::str::from_utf8(attr.key.as_ref())
                            .map_err(|e| PyException::new_err(format!("Non-UTF8 attribute key: {}", e)))?;
                        let value = attr.unescape_value()
                            .map_err(|e| PyException::new_err(format!("Value unescape error: {}", e)))?;
                        dict.set_item(key, value.as_ref())?;
                    }
                    buf.clear();
                    return Ok(Some(dict.into()));
                }

                Event::Start(ref e) if e.name().as_ref() == row_tag.as_slice() => {
                    row.clear();

                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| PyException::new_err(format!("Attribute error: {}", e)))?;
                        let key = std::str::from_utf8(attr.key.as_ref())
                            .map_err(|e| PyException::new_err(format!("Non-UTF8 attribute key: {}", e)))?;
                        let value = attr.unescape_value()
                            .map_err(|e| PyException::new_err(format!("Value unescape error: {}", e)))?;
                        row.push((key.to_owned(), value.into_owned()));
                    }

                    loop {
                        let child_event = reader.read_event_into(buf).map_err(|e| {
                            PyException::new_err(format!("XML parse error: {}", e))
                        })?;

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
                                                if let Ok(value) = attr.unescape_value() {
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
                                            let inner = reader.read_event_into(inner_buf).map_err(|e| {
                                                PyException::new_err(format!("XML parse error: {}", e))
                                            })?;
                                            match inner {
                                                Event::Start(ref inner_child) | Event::Empty(ref inner_child) => {
                                                    let inner_child_name = inner_child.name();
                                                    let inner_tag = inner_child_name.as_ref();
                                                    if inner_tag == b"FormattedValue" || inner_tag == b"Value" {
                                                        if matches!(inner, Event::Start(_)) {
                                                            let text_event = reader.read_event_into(inner_buf).map_err(|e| {
                                                                PyException::new_err(format!("Text read error: {}", e))
                                                            })?;
                                                            if let Event::Text(txt) = text_event {
                                                                text = txt.unescape()
                                                                    .map_err(|e| PyException::new_err(format!("Text unescape error: {}", e)))?
                                                                    .into_owned();
                                                            }
                                                        }
                                                        inner_buf.clear();
                                                    }
                                                }
                                                Event::End(ref e) if e.name().as_ref() == field_end_bytes => {
                                                    break;
                                                }
                                                Event::Eof => return Ok(None),
                                                _ => {}
                                            }
                                        }
                                    }
                                    row.push((key, text));
                                }

                                else if child_tag == b"Text" {
                                    let mut text_name: Option<String> = None;
                                    for attr in child.attributes() {
                                        if let Ok(attr) = attr {
                                            if attr.key.as_ref() == b"Name" {
                                                if let Ok(value) = attr.unescape_value() {
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
                                            let inner = reader.read_event_into(inner_buf).map_err(|e| {
                                                PyException::new_err(format!("XML parse error: {}", e))
                                            })?;
                                            match inner {
                                                Event::Start(ref inner_child) | Event::Empty(ref inner_child) => {
                                                    if inner_child.name().as_ref() == b"TextValue" {
                                                        if matches!(inner, Event::Start(_)) {
                                                            let text_event = reader.read_event_into(inner_buf).map_err(|e| {
                                                                PyException::new_err(format!("Text read error: {}", e))
                                                            })?;
                                                            if let Event::Text(txt) = text_event {
                                                                text = txt.unescape()
                                                                    .map_err(|e| PyException::new_err(format!("Text unescape error: {}", e)))?
                                                                    .into_owned();
                                                            }
                                                        }
                                                        inner_buf.clear();
                                                    }
                                                }
                                                Event::End(ref e) if e.name().as_ref() == text_end_bytes => {
                                                    break;
                                                }
                                                Event::Eof => return Ok(None),
                                                _ => {}
                                            }
                                        }
                                    }
                                    row.push((key, text));
                                }

                                else {
                                    let key = std::str::from_utf8(child_tag)
                                        .map_err(|e| PyException::new_err(format!("Non-UTF8 tag name: {}", e)))?
                                        .to_owned();
                                    let text = if matches!(child_event, Event::Start(_)) {
                                        let text_event = reader.read_event_into(buf).map_err(|e| {
                                            PyException::new_err(format!("Text read error: {}", e))
                                        })?;
                                        match text_event {
                                            Event::Text(txt) => txt.unescape()
                                                .map_err(|e| PyException::new_err(format!("Text unescape error: {}", e)))?
                                                .into_owned(),
                                            _ => String::new(),
                                        }
                                    } else {
                                        String::new()
                                    };
                                    row.push((key, text));
                                }
                            }

                            Event::End(ref e) if e.name().as_ref() == row_tag.as_slice() => {
                                break;
                            }
                            Event::Eof => return Ok(None),
                            _ => {}
                        }
                    }

                    let dict = PyDict::new(py);
                    for (k, v) in row.drain(..) {
                        dict.set_item(k, v)?;
                    }
                    return Ok(Some(dict.into()));
                }

                Event::Eof => return Ok(None),
                _ => {}
            }
        }
    }

    #[cfg(feature = "columnar")]
    #[staticmethod]
    fn read_to_columnar(path: String, row_tag: Option<String>) -> PyResult<PyObject> {
        let p = Path::new(&path);
        if !p.is_file() {
            return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
        }
        let mut file =
            File::open(p).map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| PyIOError::new_err(format!("Read error: {}", e)))?;

        let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();

        let mut engine = columnar::ColumnarEngine::with_capacity(bytes.len() / 512);
        engine
            .parse_bytes(&bytes, &row_tag)
            .map_err(|e| PyException::new_err(format!("Columnar parse error: {}", e)))?;

        Python::with_gil(|py| engine.to_pyarrow_table(py))
    }

    #[cfg(feature = "columnar")]
    #[staticmethod]
    fn read_to_columnar_multi(
        path: String,
        row_tag: Option<String>,
        num_chunks: usize,
    ) -> PyResult<PyObject> {
        let p = Path::new(&path);
        if !p.is_file() {
            return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
        }
        let mut file =
            File::open(p).map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| PyIOError::new_err(format!("Read error: {}", e)))?;

        let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();

        let chunks = splitter::compute_splits(&bytes, &row_tag, num_chunks);
        let mut merged = columnar::ColumnarEngine::new();

        for chunk in &chunks {
            let estimated = if chunk.len() > 0 {
                chunk.len() / 512
            } else {
                64
            };
            let mut engine =
                columnar::ColumnarEngine::with_capacity(estimated.max(1));
            engine
                .parse_bytes(&bytes[chunk.clone()], &row_tag)
                .map_err(|e| PyException::new_err(format!(
                    "Columnar parse error in chunk {:?}: {}", chunk, e
                )))?;
            merged.extend(engine);
        }

        let table = Python::with_gil(|py| merged.to_pyarrow_table(py))?;
        Ok(table)
    }

    #[cfg(feature = "columnar")]
    #[staticmethod]
    fn read_to_columnar_par(
        path: String,
        row_tag: Option<String>,
        num_chunks: usize,
    ) -> PyResult<PyObject> {
        let p = Path::new(&path);
        if !p.is_file() {
            return Err(PyIOError::new_err(format!("Not a regular file: {}", path)));
        }
        let mut file =
            File::open(p).map_err(|e| PyIOError::new_err(format!("Cannot open {}: {}", path, e)))?;

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| PyIOError::new_err(format!("Read error: {}", e)))?;

        let row_tag = row_tag.unwrap_or_else(|| "Row".to_string()).into_bytes();

        let chunks = splitter::compute_splits(&bytes, &row_tag, num_chunks);

        use rayon::prelude::*;
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let results: Vec<Result<columnar::ColumnarEngine, String>> = chunks
            .par_iter()
            .map(|range| {
                catch_unwind(AssertUnwindSafe(|| {
                    let est = if range.len() > 0 {
                        (range.len() / 512).max(64)
                    } else {
                        64
                    };
                    let mut engine = columnar::ColumnarEngine::with_capacity(est);
                    engine
                        .parse_bytes(&bytes[range.clone()], &row_tag)
                        .map_err(|e| format!("Parse error in chunk {:?}: {}", range, e))?;
                    Ok(engine)
                }))
                .unwrap_or_else(|_| {
                    Err("Worker panicked during parallel parse".to_string())
                })
            })
            .collect();

        let mut merged = columnar::ColumnarEngine::new();
        for result in results {
            let engine = result.map_err(|e| PyException::new_err(e))?;
            merged.extend(engine);
        }

        Python::with_gil(|py| merged.to_pyarrow_table(py))
    }
}

#[pymodule]
fn _crxml_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<CrxmlReader>()?;
    Ok(())
}
