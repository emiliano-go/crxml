use pyo3::exceptions::{PyIOError, PyException};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

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
                        let key = unsafe { std::str::from_utf8_unchecked(attr.key.as_ref()) };
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
                        let key = unsafe { std::str::from_utf8_unchecked(attr.key.as_ref()) };
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
                                    let key = unsafe { std::str::from_utf8_unchecked(child_tag) }.to_owned();
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
                    for (k, v) in std::mem::take(row) {
                        dict.set_item(k, v)?;
                    }
                    return Ok(Some(dict.into()));
                }

                Event::Eof => return Ok(None),
                _ => {}
            }
        }
    }
}

#[pymodule]
fn _crxml_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<CrxmlReader>()?;
    Ok(())
}
