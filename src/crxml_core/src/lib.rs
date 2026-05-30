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
            row_tag,
        })
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(mut slf: PyRefMut<'_, Self>) -> PyResult<Option<PyObject>> {
        let py = slf.py();
        let CrxmlReader { reader, buf, row_tag } = &mut *slf;

        loop {
            let event = reader.read_event_into(buf).map_err(|e| {
                PyException::new_err(format!("XML parse error: {}", e))
            })?;

            match event {
                Event::Empty(ref e) if e.name().as_ref() == row_tag.as_slice() => {
                    let mut row: Vec<(String, String)> = Vec::with_capacity(16);

                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| PyException::new_err(format!("Attribute error: {}", e)))?;
                        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
                        let value = attr.unescape_value()
                            .map_err(|e| PyException::new_err(format!("Value unescape error: {}", e)))?
                            .into_owned();
                        row.push((key, value));
                    }
                    buf.clear();

                    let dict = PyDict::new(py);
                    for (k, v) in row {
                        dict.set_item(k, v)?;
                    }
                    return Ok(Some(dict.into()));
                }

                Event::Start(ref e) if e.name().as_ref() == row_tag.as_slice() => {
                    let mut row: Vec<(String, String)> = Vec::with_capacity(16);

                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| PyException::new_err(format!("Attribute error: {}", e)))?;
                        let key = String::from_utf8_lossy(attr.key.as_ref()).into_owned();
                        let value = attr.unescape_value()
                            .map_err(|e| PyException::new_err(format!("Value unescape error: {}", e)))?
                            .into_owned();
                        row.push((key, value));
                    }
                    buf.clear();

                    loop {
                        let child_event = reader.read_event_into(buf).map_err(|e| {
                            PyException::new_err(format!("XML parse error: {}", e))
                        })?;

                        match child_event {
                            Event::Start(ref child) | Event::Empty(ref child) => {
                                let child_name = String::from_utf8_lossy(child.name().as_ref()).into_owned();

                                if child_name == "Field" {
                                    let mut field_name: Option<String> = None;
                                    for attr in child.attributes() {
                                        if let Ok(attr) = attr {
                                            let attr_key = String::from_utf8_lossy(attr.key.as_ref());
                                            if attr_key == "FieldName" || attr_key == "Name" {
                                                if let Ok(value) = attr.unescape_value() {
                                                    field_name = Some(value.into_owned());
                                                }
                                            }
                                        }
                                    }
                                    let key = field_name.unwrap_or_else(|| "Field".to_string());

                                    let mut text = String::new();
                                    if matches!(child_event, Event::Start(_)) {
                                        let mut inner_buf = Vec::new();
                                        loop {
                                            let inner = reader.read_event_into(&mut inner_buf).map_err(|e| {
                                                PyException::new_err(format!("XML parse error: {}", e))
                                            })?;
                                            match inner {
                                                Event::Start(ref inner_child) | Event::Empty(ref inner_child) => {
                                                    let inner_name = String::from_utf8_lossy(inner_child.name().as_ref()).into_owned();
                                                    if inner_name == "FormattedValue" || inner_name == "Value" {
                                                        if matches!(inner, Event::Start(_)) {
                                                            let text_event = reader.read_event_into(&mut inner_buf).map_err(|e| {
                                                                PyException::new_err(format!("Text read error: {}", e))
                                                            })?;
                                                            if let Event::Text(txt) = text_event {
                                                                text = txt.unescape()
                                                                    .map_err(|e| PyException::new_err(format!("Text unescape error: {}", e)))?
                                                                    .into_owned();
                                                            }
                                                        }
                                                    }
                                                    inner_buf.clear();
                                                }
                                                Event::End(ref e) if e.name().as_ref() == child.name().as_ref() => {
                                                    inner_buf.clear();
                                                    break;
                                                }
                                                Event::Eof => return Ok(None),
                                                _ => {
                                                    inner_buf.clear();
                                                }
                                            }
                                        }
                                    }
                                    row.push((key, text));
                                    buf.clear();
                                }

                                else if child_name == "Text" {
                                    let mut text_name: Option<String> = None;
                                    for attr in child.attributes() {
                                        if let Ok(attr) = attr {
                                            let attr_key = String::from_utf8_lossy(attr.key.as_ref());
                                            if attr_key == "Name" {
                                                if let Ok(value) = attr.unescape_value() {
                                                    text_name = Some(value.into_owned());
                                                }
                                            }
                                        }
                                    }
                                    let key = text_name.unwrap_or_else(|| "Text".to_string());

                                    let mut text = String::new();
                                    if matches!(child_event, Event::Start(_)) {
                                        let mut inner_buf = Vec::new();
                                        loop {
                                            let inner = reader.read_event_into(&mut inner_buf).map_err(|e| {
                                                PyException::new_err(format!("XML parse error: {}", e))
                                            })?;
                                            match inner {
                                                Event::Start(ref inner_child) | Event::Empty(ref inner_child) => {
                                                    let inner_name = String::from_utf8_lossy(inner_child.name().as_ref()).into_owned();
                                                    if inner_name == "TextValue" {
                                                        if matches!(inner, Event::Start(_)) {
                                                            let text_event = reader.read_event_into(&mut inner_buf).map_err(|e| {
                                                                PyException::new_err(format!("Text read error: {}", e))
                                                            })?;
                                                            if let Event::Text(txt) = text_event {
                                                                text = txt.unescape()
                                                                    .map_err(|e| PyException::new_err(format!("Text unescape error: {}", e)))?
                                                                    .into_owned();
                                                            }
                                                        }
                                                    }
                                                    inner_buf.clear();
                                                }
                                                Event::End(ref e) if e.name().as_ref() == child.name().as_ref() => {
                                                    inner_buf.clear();
                                                    break;
                                                }
                                                Event::Eof => return Ok(None),
                                                _ => {
                                                    inner_buf.clear();
                                                }
                                            }
                                        }
                                    }
                                    row.push((key, text));
                                    buf.clear();
                                }

                                else {
                                    let key = child_name;
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
                                    buf.clear();
                                }
                            }

                            Event::End(ref e) if e.name().as_ref() == row_tag.as_slice() => {
                                buf.clear();
                                break;
                            }
                            Event::Eof => return Ok(None),
                            _ => {
                                buf.clear();
                            }
                        }
                    }

                    let dict = PyDict::new(py);
                    for (k, v) in row {
                        dict.set_item(k, v)?;
                    }
                    return Ok(Some(dict.into()));
                }

                Event::Eof => return Ok(None),
                _ => {
                    buf.clear();
                }
            }
        }
    }
}

#[pymodule]
fn _crxml_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<CrxmlReader>()?;
    Ok(())
}