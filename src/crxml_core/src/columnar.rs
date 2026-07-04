use pyo3::prelude::*;
use pyo3::types::PyDict;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::collections::HashMap;
use std::io::Cursor;

/// Per-column builder: stores all values as optional owned strings.
struct ColumnBuilder {
    values: Vec<Option<String>>,
}

impl ColumnBuilder {
    fn with_capacity(cap: usize) -> Self {
        ColumnBuilder {
            values: Vec::with_capacity(cap),
        }
    }

    fn push(&mut self, value: Option<String>) {
        self.values.push(value);
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn to_pylist<'a>(&self, py: Python<'a>) -> PyResult<Bound<'a, PyList>> {
        let items: Vec<PyObject> = self
            .values
            .iter()
            .map(|v| match v {
                Some(s) => s.into_py(py),
                None => py.None(),
            })
            .collect();
        PyList::new(py, &items)
    }
}

use pyo3::types::PyList;

/// Columnar engine: parses XML rows into column-oriented storage,
/// then exports to a PyArrow table in one shot.
pub struct ColumnarEngine {
    columns: HashMap<String, ColumnBuilder>,
    column_order: Vec<String>,
    row_count: usize,
    estimated_rows: usize,
}

impl ColumnarEngine {
    pub fn new() -> Self {
        ColumnarEngine {
            columns: HashMap::new(),
            column_order: Vec::new(),
            row_count: 0,
            estimated_rows: 0,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        ColumnarEngine {
            columns: HashMap::new(),
            column_order: Vec::new(),
            row_count: 0,
            estimated_rows: cap,
        }
    }

    fn ensure_column(&mut self, name: &str) {
        if !self.columns.contains_key(name) {
            let est = self.estimated_rows.max(64);
            let mut b = ColumnBuilder::with_capacity(est);
            for _ in 0..self.row_count {
                b.push(None);
            }
            self.columns.insert(name.to_owned(), b);
            self.column_order.push(name.to_owned());
        }
    }

    fn push_field(&mut self, name: &str, value: Option<String>) {
        self.ensure_column(name);
        if let Some(b) = self.columns.get_mut(name) {
            // Last-write-wins: if this column was already pushed for the
            // current (incomplete) row, overwrite instead of append.
            if b.len() > self.row_count {
                b.values.pop();
            }
            b.push(value);
        }
    }

    /// Null-fill any column missing this row.
    fn finish_row(&mut self) {
        let target = self.row_count + 1;
        for b in self.columns.values_mut() {
            while b.len() < target {
                b.push(None);
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
    pub fn parse_bytes(&mut self, bytes: &[u8], row_tag: &[u8]) -> Result<(), String> {
        let mut reader = Reader::from_reader(Cursor::new(bytes));
        reader.config_mut().check_end_names = false;
        let mut buf = Vec::with_capacity(4096);
        let mut inner_buf = Vec::with_capacity(4096);

        let row_tag_owned = row_tag.to_vec();

        loop {
            let event = match reader.read_event_into(&mut buf) {
                Ok(e) => e,
                Err(_) => {
                    let err_pos = reader.buffer_position() as usize;
                    self.parse_tail(bytes, row_tag, err_pos)?;
                    return Ok(());
                }
            };

            match event {
                Event::Empty(ref e) if e.name().as_ref() == row_tag_owned => {
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| e.to_string())?;
                        let key = std::str::from_utf8(attr.key.as_ref())
                            .map_err(|e| e.to_string())?;
                        let value = attr
                            .unescape_value()
                            .map_err(|e| e.to_string())?
                            .into_owned();
                        self.push_field(key, Some(value));
                    }
                    self.finish_row();
                    buf.clear();
                }

                Event::Start(ref e) if e.name().as_ref() == row_tag_owned => {
                    for attr in e.attributes() {
                        let attr = attr.map_err(|e| e.to_string())?;
                        let key = std::str::from_utf8(attr.key.as_ref())
                            .map_err(|e| e.to_string())?;
                        let value = attr
                            .unescape_value()
                            .map_err(|e| e.to_string())?
                            .into_owned();
                        self.push_field(key, Some(value));
                    }

                    loop {
                        let child_event = reader
                            .read_event_into(&mut buf)
                            .map_err(|e| e.to_string())?;

                        match child_event {
                            Event::Start(ref child) | Event::Empty(ref child) => {
                                let child_name = child.name();
                                let child_tag = child_name.as_ref();

                                if child_tag == b"Field" {
                                    let mut field_name: Option<String> = None;
                                    for attr in child.attributes() {
                                        if let Ok(attr) = attr {
                                            let attr_key = attr.key.as_ref();
                                            if attr_key == b"FieldName"
                                                || attr_key == b"Name"
                                            {
                                                if let Ok(value) = attr.unescape_value() {
                                                    field_name = Some(value.into_owned());
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    let key =
                                        field_name.unwrap_or_else(|| "Field".to_string());

                                    let mut text = String::new();
                                    if matches!(child_event, Event::Start(_)) {
                                        let field_end_bytes = child_name.as_ref().to_vec();
                                        loop {
                                            let inner = reader
                                                .read_event_into(&mut inner_buf)
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
                                                                .read_event_into(
                                                                    &mut inner_buf,
                                                                )
                                                                .map_err(|e| {
                                                                    e.to_string()
                                                                })?;
                                                            if let Event::Text(txt) =
                                                                text_event
                                                            {
                                                                text = txt
                                                                    .unescape()
                                                                    .map_err(|e| {
                                                                        e.to_string()
                                                                    })?
                                                                    .into_owned();
                                                            }
                                                        }
                                                        inner_buf.clear();
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
                                    self.push_field(&key, Some(text));
                                } else if child_tag == b"Text" {
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
                                    let key =
                                        text_name.unwrap_or_else(|| "Text".to_string());

                                    let mut text = String::new();
                                    if matches!(child_event, Event::Start(_)) {
                                        let text_end_bytes =
                                            child_name.as_ref().to_vec();
                                        loop {
                                            let inner = reader
                                                .read_event_into(&mut inner_buf)
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
                                                                .read_event_into(
                                                                    &mut inner_buf,
                                                                )
                                                                .map_err(|e| {
                                                                    e.to_string()
                                                                })?;
                                                            if let Event::Text(txt) =
                                                                text_event
                                                            {
                                                                text = txt
                                                                    .unescape()
                                                                    .map_err(|e| {
                                                                        e.to_string()
                                                                    })?
                                                                    .into_owned();
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
                                                Event::Eof => return Ok(()),
                                                _ => {}
                                            }
                                        }
                                    }
                                    self.push_field(&key, Some(text));
                                } else if child_tag == b"Section" {
                                    // Section carries SectionNumber; extract it.
                                    let sn = child
                                        .attributes()
                                        .filter_map(|a| a.ok())
                                        .find(|a| a.key.as_ref() == b"SectionNumber")
                                        .and_then(|a| a.unescape_value().ok())
                                        .unwrap_or_default()
                                        .into_owned();
                                    self.push_field("Section", Some(sn));
                                } else {
                                    // Unknown tag: push tag name with empty value.
                                    let key = std::str::from_utf8(child_tag)
                                        .map_err(|e| e.to_string())?
                                        .to_owned();
                                    self.push_field(&key, Some(String::new()));
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
                    buf.clear();
                }

                Event::Eof => return Ok(()),
                _ => {}
            }
        }
    }

    /// Fallback: scan bytes for remaining row tags and parse each row with
    /// an independent quick-xml reader.  Tag names are copied to owned storage
    /// before nested reads into the same buffer to avoid borrow conflicts.
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
                                                    let ic_name =
                                                        ic.name().as_ref().to_vec();
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
                                                    let ic_name =
                                                        ic.name().as_ref().to_vec();
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
                                                Ok(Event::End(ref ne))
                                                    if ne.name().as_ref() == end =>
                                                {
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
                                        loop {
                                            match rr.read_event_into(&mut buf) {
                                                Ok(Event::End(ref ne))
                                                    if ne.name().as_ref() == end =>
                                                {
                                                    break;
                                                }
                                                Ok(Event::Text(txt)) => {
                                                    if let Ok(v) = txt.unescape() {
                                                        self.push_field(
                                                            &name,
                                                            Some(v.into_owned()),
                                                        );
                                                    }
                                                }
                                                Ok(Event::Eof) => return Ok(()),
                                                _ => {}
                                            }
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
                                    let key =
                                        std::str::from_utf8(&tag)
                                            .unwrap_or("")
                                            .to_owned();
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
    pub fn extend(&mut self, other: ColumnarEngine) {
        let self_rows = self.row_count;
        let other_rows = other.row_count;

        // 1. Create columns from other that self doesn't have yet
        //    (null-padded for self's existing rows, no values copied yet)
        for name in &other.column_order {
            if !self.column_order.contains(name) {
                let est = self_rows + other.estimated_rows.max(64);
                let mut builder = ColumnBuilder::with_capacity(est);
                for _ in 0..self_rows {
                    builder.push(None);
                }
                self.columns.insert(name.clone(), builder);
                self.column_order.push(name.clone());
            }
        }

        // 2. Append other's values to all columns, null-pad missing ones
        for name in &self.column_order.clone() {
            if let Some(self_b) = self.columns.get_mut(name) {
                if let Some(other_b) = other.columns.get(name) {
                    for val in &other_b.values {
                        self_b.push(val.clone());
                    }
                } else {
                    for _ in 0..other_rows {
                        self_b.push(None);
                    }
                }
            }
        }

        self.row_count = self_rows + other_rows;
    }

    /// Build a PyArrow table from the columnar data by calling
    /// `pyarrow.table({"col": pa.array([...]), ...})` from Python.
    pub fn to_pyarrow_table(&self, py: Python<'_>) -> PyResult<PyObject> {
        let pyarrow = PyModule::import(py, "pyarrow")?;

        let dict = PyDict::new(py);
        for name in &self.column_order {
            if let Some(b) = self.columns.get(name) {
                let py_list = b.to_pylist(py)?;
                let array = pyarrow.call_method1("array", (py_list,))?;
                dict.set_item(name.as_str(), array)?;
            }
        }
        let table = pyarrow.call_method1("table", (dict,))?;
        Ok(table.into())
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
        merged.extend(e1);
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
            merged.extend(engine);
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
            merged.extend(engine);
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
            merged.extend(engine);
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
            merged.extend(engine);
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
            score_col.values[0],
            Some("20".to_string()),
            "last-write-wins: expected '20' got {:?}",
            score_col.values[0]
        );
    }
}
