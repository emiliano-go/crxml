//! Crystal Reports XML decoder.

use rypipe_core::RecordParser;

use super::scanner;

/// Decoder for Crystal Reports XML row streams.
///
/// Parses `<Row ...>` elements (or a configurable row tag) into field events
/// that can be fed into any `rypipe_core::ColumnarSink`, typically a
/// [`TableBuilder`](rypipe_core::engine::TableBuilder).
///
/// Parsing uses the hand-rolled memchr scanner in [`scanner`]; no XML
/// tokenizer runs on this path. Fields whose resolved column name the
/// execution plan drops are skipped at the byte level before any value is
/// extracted.
#[derive(Clone, Debug, Default)]
pub struct CrystalXmlDecoder {
    row_tag: Vec<u8>,
}

impl CrystalXmlDecoder {
    /// Create a decoder with a custom row element name.
    pub fn with_row_tag(row_tag: impl AsRef<[u8]>) -> Self {
        Self {
            row_tag: row_tag.as_ref().to_vec(),
        }
    }
}

impl RecordParser for CrystalXmlDecoder {
    fn validate(&self, bytes: &[u8]) -> rypipe_core::Result<()> {
        simdutf8::basic::from_utf8(bytes)?;
        Ok(())
    }

    fn parse_chunk(&self, bytes: &[u8], sink: &mut dyn ColumnarSink) -> rypipe_core::Result<()> {
        scanner::scan_chunk(bytes, &self.row_tag, sink)
    }

    #[inline]
    fn parse_chunk_generic<S: ColumnarSink>(
        &self,
        bytes: &[u8],
        sink: &mut S,
    ) -> rypipe_core::Result<()> {
        scanner::scan_chunk(bytes, &self.row_tag, sink)
    }
}

use rypipe_core::ColumnarSink;

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::AsArray;
    use rypipe_core::engine::TableBuilder;

    fn parse(xml: &[u8]) -> TableBuilder {
        let mut sink = TableBuilder::with_capacity(4);
        CrystalXmlDecoder::with_row_tag(b"Row")
            .parse_chunk_generic(xml, &mut sink)
            .unwrap();
        sink
    }

    #[test]
    fn test_row_attributes() {
        let xml = br#"<Rows><Row A="1" B="hello"/></Rows>"#;
        let mut sink = parse(xml);
        assert_eq!(sink.num_rows(), 1);
        let batch = sink.finish().unwrap();
        let a = batch.column_by_name("A").unwrap().as_string::<i32>();
        let b = batch.column_by_name("B").unwrap().as_string::<i32>();
        assert_eq!(a.value(0), "1");
        assert_eq!(b.value(0), "hello");
    }

    #[test]
    fn test_field_child() {
        let xml = br#"<Row><Field Name="X"><Value>42</Value></Field></Row>"#;
        let mut sink = parse(xml);
        assert_eq!(sink.num_rows(), 1);
        let batch = sink.finish().unwrap();
        let x = batch.column_by_name("X").unwrap().as_string::<i32>();
        assert_eq!(x.value(0), "42");
    }

    #[test]
    fn test_field_both_children_value_wins() {
        let xml = br#"<Row><Field FieldName="X"><FormattedValue>abc</FormattedValue><Value>42</Value></Field></Row>"#;
        let mut sink = parse(xml);
        let batch = sink.finish().unwrap();
        let x = batch.column_by_name("X").unwrap().as_string::<i32>();
        assert_eq!(x.value(0), "42");
    }

    #[test]
    fn test_text_child() {
        let xml = br#"<Row><Text Name="Title"><TextValue>Report</TextValue></Text></Row>"#;
        let mut sink = parse(xml);
        assert_eq!(sink.num_rows(), 1);
        let batch = sink.finish().unwrap();
        let title = batch.column_by_name("Title").unwrap().as_string::<i32>();
        assert_eq!(title.value(0), "Report");
    }

    #[test]
    fn test_section_child() {
        let xml = br#"<Row><Section SectionNumber="3"/></Row>"#;
        let mut sink = parse(xml);
        assert_eq!(sink.num_rows(), 1);
        let batch = sink.finish().unwrap();
        let section = batch.column_by_name("Section").unwrap().as_string::<i32>();
        assert_eq!(section.value(0), "3");
    }

    #[test]
    fn test_unknown_child() {
        let xml = br#"<Row><Custom/></Row>"#;
        let mut sink = parse(xml);
        assert_eq!(sink.num_rows(), 1);
        let batch = sink.finish().unwrap();
        let custom = batch.column_by_name("Custom").unwrap().as_string::<i32>();
        assert_eq!(custom.value(0), "");
    }

    #[test]
    fn test_empty_input() {
        let sink = parse(b"");
        assert_eq!(sink.num_rows(), 0);
    }

    #[test]
    fn test_partial_trailing_row_discarded() {
        let xml = br#"<Row><Field Name="X"><Value>1</Value></Field></Row><Row><Field Name="X""#;
        let mut sink = parse(xml);
        assert_eq!(sink.num_rows(), 1);
        let batch = sink.finish().unwrap();
        let x = batch.column_by_name("X").unwrap().as_string::<i32>();
        assert_eq!(x.value(0), "1");
    }

    #[test]
    fn test_dropped_field_not_in_output() {
        let plan = rypipe_core::ExecutionPlan::new().drop("DropMe");
        let mut sink = TableBuilder::with_plan(4, std::sync::Arc::new(plan));
        let xml = br#"<Row><Field Name="Keep"><Value>1</Value></Field><Field Name="DropMe"><Value>x</Value></Field></Row>"#;
        CrystalXmlDecoder::with_row_tag(b"Row")
            .parse_chunk_generic(xml, &mut sink)
            .unwrap();
        let batch = sink.finish().unwrap();
        assert!(batch.column_by_name("DropMe").is_none());
        let keep = batch.column_by_name("Keep").unwrap().as_string::<i32>();
        assert_eq!(keep.value(0), "1");
    }

    #[test]
    fn test_entities_unescaped() {
        let xml = br#"<Row><Field FieldName="E"><FormattedValue>A &amp; B</FormattedValue></Field></Row>"#;
        let mut sink = parse(xml);
        let batch = sink.finish().unwrap();
        let e = batch.column_by_name("E").unwrap().as_string::<i32>();
        assert_eq!(e.value(0), "A & B");
    }

    #[test]
    fn test_filter_eq_keep_rows() {
        let plan = rypipe_core::ExecutionPlan::new().filter_eq("S", "keep");
        let mut sink = TableBuilder::with_plan(4, std::sync::Arc::new(plan));
        let xml = br#"<Rows>
<Row><Field Name="S"><Value>keep</Value></Field><Field Name="I"><Value>1</Value></Field></Row>
<Row><Field Name="S"><Value>drop</Value></Field><Field Name="I"><Value>2</Value></Field></Row>
<Row><Field Name="S"><Value>keep</Value></Field><Field Name="I"><Value>3</Value></Field></Row>
<Row><Field Name="S"><Value>keep</Value></Field><Field Name="I"><Value>4</Value></Field></Row>
<Row><Field Name="S"><Value>drop</Value></Field><Field Name="I"><Value>5</Value></Field></Row>
<Row><Field Name="S"><Value>keep</Value></Field><Field Name="I"><Value>6</Value></Field></Row>
</Rows>"#;
        CrystalXmlDecoder::with_row_tag(b"Row")
            .parse_chunk_generic(xml, &mut sink)
            .unwrap();
        let batch = sink.finish().unwrap();
        assert_eq!(batch.num_rows(), 4, "expected 4 keep rows");
    }
}
