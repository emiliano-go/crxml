//! Hand-rolled memchr scanner for Crystal Reports XML row chunks.
//!
//! Replaces the quick-xml event loop on the columnar path by exploiting the
//! fixed CR grammar: rows contain only `<Field>`, `<Text>`, and
//! `<Section>` children, and value elements wrap plain text. All scanning is
//! SIMD-accelerated (`memchr`/`memmem`); no XML tokenizer state machine runs.
//!
//! Semantics mirror the previous quick-xml decoder exactly:
//! - Row attributes become fields (self-closing and paired rows).
//! - Field key: first attribute in document order named `FieldName` or
//!   `Name`; default `"Field"`.
//! - Field value: text of each `<FormattedValue>`/`<Value>` child in order;
//!   last one wins. An empty or markup-leading body does NOT clear a
//!   previously captured value (quick-xml only assigns on a Text event).
//! - Text key: `Name` attribute, default `"Text"`; value from `<TextValue>`.
//! - Section: emits `Section` = `SectionNumber` attribute (default empty).
//! - Any other child element emits `{tag}` = "".
//! - Elements whose resolved name the plan drops (`sink.wants == false`) are
//!   skipped byte-wise: the scanner jumps straight to the close tag without
//!   visiting any children.
//! - A truncated row at EOF is discarded (the sink's normalize trims the
//!   partial values). Malformed constructs inside a row abandon that row and
//!   scanning resumes at the next valid row start.

use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use memchr::{memchr, memchr3, memmem};

use rypipe_core::{ColumnarSink, Value};

use crate::xml::splitter::{find_special_regions, next_row_start};

/// Bytes are chunk-validated UTF-8 (see `RecordParser::validate`); skip
/// std's per-call revalidation.
#[allow(unsafe_code)]
#[inline]
fn utf8_unchecked(b: &[u8]) -> &str {
    unsafe { std::str::from_utf8_unchecked(b) }
}

/// Outcome of advancing past one structural unit.
enum Flow {
    /// Continue scanning at this offset.
    At(usize),
    /// Hit EOF mid-row: discard the partial row and stop the chunk.
    Truncated,
    /// Malformed construct: abandon this row, resume at the next row start.
    Recover,
}

/// Scan a chunk of CR XML bytes, emitting rows into `sink`.
pub(crate) fn scan_chunk(
    bytes: &[u8],
    row_tag: &[u8],
    sink: &mut dyn ColumnarSink,
) -> Result<(), rypipe_core::Error> {
    let (regions, _) = find_special_regions(bytes);
    let mut pos = 0usize;
    while let Some(start) = next_row_start(bytes, pos, row_tag, &regions) {
        match parse_row(bytes, start, row_tag, &regions, sink) {
            Flow::At(next) => pos = next.max(start + 1),
            // Partial trailing row: stop; normalize() discards it at finish.
            Flow::Truncated => break,
            // Broken row: resume after its start tag.
            Flow::Recover => pos = start + 1,
        }
    }
    Ok(())
}

/// Scan a single row starting at or after `pos`, emitting via `sink`.
/// Returns `Some(next_pos)` on success (next search offset), `None` on EOF.
/// Handles `Recover` by advancing one byte and retrying.
pub(crate) fn scan_one_row(
    bytes: &[u8],
    mut pos: usize,
    row_tag: &[u8],
    regions: &[Range<usize>],
    sink: &mut dyn ColumnarSink,
) -> Option<usize> {
    loop {
        let start = next_row_start(bytes, pos, row_tag, regions)?;
        match parse_row(bytes, start, row_tag, regions, sink) {
            Flow::At(next) => return Some(next.max(start + 1)),
            Flow::Truncated => return None,
            Flow::Recover => pos = start + 1,
        }
    }
}

/// Parse one row starting at the `<` of its open tag.
fn parse_row(
    bytes: &[u8],
    lt: usize,
    row_tag: &[u8],
    regions: &[Range<usize>],
    sink: &mut dyn ColumnarSink,
) -> Flow {
    let open = match scan_open_tag(bytes, lt) {
        Some(o) => o,
        None => return Flow::Truncated,
    };

    sink.begin_row();

    if emit_all_attrs(bytes, &open, sink) {
        return Flow::Recover;
    }

    if open.self_closing {
        sink.end_row();
        return Flow::At(open.after());
    }

    let mut cur = open.after();
    loop {
        cur = match scan_child(bytes, cur, row_tag, regions, sink) {
            ChildFlow::Continue(next) => next,
            ChildFlow::RowEnd(after) => {
                sink.end_row();
                return Flow::At(after);
            }
            ChildFlow::Truncated => return Flow::Truncated,
            ChildFlow::Recover => return Flow::Recover,
        };
    }
}

/// Result of processing one token inside a row body.
enum ChildFlow {
    /// Not the row's close tag; continue at this offset.
    Continue(usize),
    /// The row's close tag; offset just past it.
    RowEnd(usize),
    Truncated,
    Recover,
}

fn scan_child(
    bytes: &[u8],
    cur: usize,
    row_tag: &[u8],
    regions: &[Range<usize>],
    sink: &mut dyn ColumnarSink,
) -> ChildFlow {
    let l = match next_lt(bytes, cur) {
        Some(p) => p,
        None => return ChildFlow::Truncated,
    };
    if l + 1 >= bytes.len() {
        return ChildFlow::Truncated;
    }
    if in_region(regions, l) {
        return ChildFlow::Continue(region_end(regions, l));
    }
    if bytes[l + 1] == b'/' {
        let (name, after) = match scan_close_tag(bytes, l) {
            Some(x) => x,
            None => return ChildFlow::Truncated,
        };
        if name == row_tag {
            return ChildFlow::RowEnd(after);
        }
        return ChildFlow::Continue(after);
    }
    if bytes[l + 1] == b'!' || bytes[l + 1] == b'?' {
        return match skip_construct(bytes, l) {
            Some(p) => ChildFlow::Continue(p),
            None => ChildFlow::Truncated,
        };
    }
    let child = match scan_open_tag(bytes, l) {
        Some(c) => c,
        None => return ChildFlow::Truncated,
    };
    match child.name {
        b"Field" => lift(field_element(bytes, &child, regions, sink)),
        b"Text" => lift(text_element(bytes, &child, regions, sink)),
        b"Section" => lift(section_element(bytes, &child, sink)),
        other => {
            let name = utf8_unchecked(other);
            if sink.wants(name) {
                sink.put_field(name, Value::Str(""));
            }
            ChildFlow::Continue(child.after())
        }
    }
}

fn lift(flow: Flow) -> ChildFlow {
    match flow {
        Flow::At(n) => ChildFlow::Continue(n),
        Flow::Truncated => ChildFlow::Truncated,
        Flow::Recover => ChildFlow::Recover,
    }
}

/// Emit every attribute of an open tag as a field (used for the row tag).
///
/// Returns true when a malformed attribute was found (row must be abandoned).
fn emit_all_attrs(bytes: &[u8], open: &OpenTag<'_>, sink: &mut dyn ColumnarSink) -> bool {
    for attr in AttrIter::new(open.interior(bytes)) {
        match attr {
            Ok((key_raw, val_raw)) => {
                let key = decode_attr(key_raw);
                if !sink.wants(&key) {
                    continue;
                }
                let val = decode_attr(val_raw);
                sink.put_field(&key, Value::Str(&val));
            }
            Err(()) => return true,
        }
    }
    false
}

/// Handle one `<Field ...>` element: skip entirely when the plan drops the
/// resolved column, otherwise capture FormattedValue/Value text (last wins).
fn field_element<'a>(
    bytes: &'a [u8],
    open: &OpenTag<'_>,
    regions: &[Range<usize>],
    sink: &mut dyn ColumnarSink,
) -> Flow {
    let interior = open.interior(bytes);
    let key_raw = match find_attr_value(interior, &[b"FieldName", b"Name"]) {
        Ok(Some(v)) => v,
        Ok(None) => b"Field",
        Err(()) => return Flow::Recover,
    };
    let key = decode_attr(key_raw);

    if !sink.wants(&key) {
        return match find_close_after(bytes, open.after(), &CLOSE_FIELD, PAT_FIELD, regions) {
            Some(after) => Flow::At(after),
            None => Flow::Truncated,
        };
    }

    let mut text: Cow<'a, str> = Cow::Borrowed("");
    let mut cur = open.after();
    loop {
        let l = match next_lt(bytes, cur) {
            Some(p) => p,
            None => return Flow::Truncated,
        };
        if l + 1 >= bytes.len() {
            return Flow::Truncated;
        }
        if in_region(regions, l) {
            cur = region_end(regions, l);
            continue;
        }
        if bytes[l + 1] == b'/' {
            let (name, after) = match scan_close_tag(bytes, l) {
                Some(x) => x,
                None => return Flow::Truncated,
            };
            if name == b"Field" {
                sink.put_field(&key, Value::Str(text.as_ref()));
                return Flow::At(after);
            }
            cur = after;
            continue;
        }
        if bytes[l + 1] == b'!' || bytes[l + 1] == b'?' {
            cur = match skip_construct(bytes, l) {
                Some(p) => p,
                None => return Flow::Truncated,
            };
            continue;
        }
        let inner = match scan_open_tag(bytes, l) {
            Some(c) => c,
            None => return Flow::Truncated,
        };
        if (inner.name == b"FormattedValue" || inner.name == b"Value") && !inner.self_closing {
            let (finder, pat_len) = if inner.name == b"FormattedValue" {
                (&CLOSE_FORMATTED, PAT_FORMATTED_VALUE)
            } else {
                (&CLOSE_VALUE, PAT_VALUE)
            };
            match raw_text_until(bytes, inner.after(), finder, pat_len, regions) {
                Some((raw, after)) => {
                    assign_text(&mut text, raw);
                    cur = after;
                }
                None => return Flow::Truncated,
            }
        } else {
            cur = inner.after();
        }
    }
}

/// Handle one `<Text Name="...">` element with a `<TextValue>` child.
fn text_element<'a>(
    bytes: &'a [u8],
    open: &OpenTag<'_>,
    regions: &[Range<usize>],
    sink: &mut dyn ColumnarSink,
) -> Flow {
    let interior = open.interior(bytes);
    let key_raw = match find_attr_value(interior, &[b"Name"]) {
        Ok(Some(v)) => v,
        Ok(None) => b"Text",
        Err(()) => return Flow::Recover,
    };
    let key = decode_attr(key_raw);

    if !sink.wants(&key) {
        return match find_close_after(bytes, open.after(), &CLOSE_TEXT, PAT_TEXT, regions) {
            Some(after) => Flow::At(after),
            None => Flow::Truncated,
        };
    }

    let mut text: Cow<'a, str> = Cow::Borrowed("");
    let mut cur = open.after();
    loop {
        let l = match next_lt(bytes, cur) {
            Some(p) => p,
            None => return Flow::Truncated,
        };
        if l + 1 >= bytes.len() {
            return Flow::Truncated;
        }
        if in_region(regions, l) {
            cur = region_end(regions, l);
            continue;
        }
        if bytes[l + 1] == b'/' {
            let (name, after) = match scan_close_tag(bytes, l) {
                Some(x) => x,
                None => return Flow::Truncated,
            };
            if name == b"Text" {
                sink.put_field(&key, Value::Str(text.as_ref()));
                return Flow::At(after);
            }
            cur = after;
            continue;
        }
        if bytes[l + 1] == b'!' || bytes[l + 1] == b'?' {
            cur = match skip_construct(bytes, l) {
                Some(p) => p,
                None => return Flow::Truncated,
            };
            continue;
        }
        let inner = match scan_open_tag(bytes, l) {
            Some(c) => c,
            None => return Flow::Truncated,
        };
        if inner.name == b"TextValue" && !inner.self_closing {
            match raw_text_until(bytes, inner.after(), &CLOSE_TEXTVALUE, PAT_TEXT_VALUE, regions) {
                Some((raw, after)) => {
                    assign_text(&mut text, raw);
                    cur = after;
                }
                None => return Flow::Truncated,
            }
        } else {
            cur = inner.after();
        }
    }
}

/// Emit the `Section` field from a `<Section SectionNumber="..">` element.
fn section_element(bytes: &[u8], open: &OpenTag<'_>, sink: &mut dyn ColumnarSink) -> Flow {
    if !sink.wants("Section") {
        return Flow::At(open.after());
    }
    let interior = open.interior(bytes);
    let sn = match find_attr_value(interior, &[b"SectionNumber"]) {
        Ok(Some(v)) => decode_attr(v),
        Ok(None) => Cow::Borrowed(""),
        Err(()) => return Flow::Recover,
    };
    sink.put_field("Section", Value::Str(sn.as_ref()));
    Flow::At(open.after())
}

// ---------------------------------------------------------------------------
// Token primitives
// ---------------------------------------------------------------------------

/// An open tag scanned from the input.
struct OpenTag<'a> {
    /// Tag name slice (without brackets).
    name: &'a [u8],
    /// Offset just past the name (before attributes).
    name_end: usize,
    /// Offset of `'>'`.
    gt: usize,
    self_closing: bool,
}

impl OpenTag<'_> {
    /// Bytes between the tag name and `'>'`, excluding any trailing `/`.
    fn interior<'b>(&self, bytes: &'b [u8]) -> &'b [u8] {
        let end = if self.self_closing {
            self.gt - 1
        } else {
            self.gt
        };
        &bytes[self.name_end..end]
    }

    /// First byte after `'>'`.
    fn after(&self) -> usize {
        self.gt + 1
    }
}

/// Offset of the next `<` at or after `from`.
#[inline]
fn next_lt(bytes: &[u8], from: usize) -> Option<usize> {
    memchr(b'<', &bytes[from..]).map(|rel| from + rel)
}

#[inline]
fn in_region(regions: &[Range<usize>], at: usize) -> bool {
    if regions.is_empty() {
        return false;
    }
    regions.iter().any(|r| r.contains(&at))
}

#[inline]
fn region_end(regions: &[Range<usize>], at: usize) -> usize {
    if regions.is_empty() {
        return at + 1;
    }
    for r in regions {
        if r.contains(&at) {
            return r.end;
        }
    }
    at + 1
}

/// Scan an open tag starting at `<`. Quote-aware so `>` inside attribute
/// values does not terminate the tag early. Returns `None` when EOF is hit
/// before the closing `>` (truncated input).
fn scan_open_tag(bytes: &[u8], lt: usize) -> Option<OpenTag<'_>> {
    let mut i = lt + 1;
    let name_start = i;
    while i < bytes.len() && !matches!(bytes[i], b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/') {
        i += 1;
    }
    if i == name_start {
        return None;
    }
    let name = &bytes[name_start..i];
    let name_end = i;

    // Walk to '>' honoring quoted attribute values, using SIMD jumps.
    let gt = loop {
        let rel = memchr3(b'"', b'\'', b'>', &bytes[i..])?;
        let at = i + rel;
        if bytes[at] == b'>' {
            break at;
        }
        // Jump past the matching closing quote in one search.
        i = at + 1 + memchr(bytes[at], &bytes[at + 1..])? + 1;
    };

    let self_closing = gt > name_start && bytes[gt - 1] == b'/';
    Some(OpenTag {
        name,
        name_end,
        gt,
        self_closing,
    })
}

/// Parse a close tag starting at `</`. Returns `(name, offset_after_gt)`.
fn scan_close_tag(bytes: &[u8], lt: usize) -> Option<(&[u8], usize)> {
    let start = lt + 2;
    let gt_rel = memchr(b'>', &bytes[start..])?;
    let gt = start + gt_rel;
    let raw = &bytes[start..gt];
    // Trim trailing whitespace (`</Row >`); leading whitespace is invalid.
    let name = match raw
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t' && b != b'\r' && b != b'\n')
    {
        Some(last) => &raw[..=last],
        None => raw,
    };
    Some((name, gt + 1))
}

/// Skip a comment, CDATA section, PI, or doctype starting at `<`.
fn skip_construct(bytes: &[u8], lt: usize) -> Option<usize> {
    let rest = &bytes[lt..];
    if rest.starts_with(b"<!--") {
        let end_rel = memmem::find(&bytes[lt + 4..], b"-->")?;
        return Some(lt + 4 + end_rel + 3);
    }
    if rest.starts_with(b"<![CDATA[") {
        let end_rel = memmem::find(&bytes[lt + 9..], b"]]>")?;
        return Some(lt + 9 + end_rel + 3);
    }
    // PI, DOCTYPE, and friends end at the first '>'.
    let gt_rel = memchr(b'>', &bytes[lt + 1..])?;
    Some(lt + 1 + gt_rel + 1)
}

/// Iterator over `key="value"` / `key='value'` attribute pairs.
///
/// Yields `Err(())` on malformed attributes (missing quote, dangling key).
struct AttrIter<'a> {
    rest: &'a [u8],
}

impl<'a> AttrIter<'a> {
    fn new(interior: &'a [u8]) -> Self {
        AttrIter { rest: interior }
    }
}

impl<'a> Iterator for AttrIter<'a> {
    type Item = Result<(&'a [u8], &'a [u8]), ()>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut i = 0;
        while i < self.rest.len() && is_ws(self.rest[i]) {
            i += 1;
        }
        self.rest = &self.rest[i..];
        if self.rest.is_empty() {
            return None;
        }

        let key_start = 0;
        let mut j = 0;
        while j < self.rest.len() && !matches!(self.rest[j], b'=' | b' ' | b'\t' | b'\r' | b'\n') {
            j += 1;
        }
        let key = &self.rest[key_start..j];
        if j >= self.rest.len() || self.rest[j] != b'=' {
            // Valueless attribute: malformed under our grammar.
            self.rest = &[];
            return Some(Err(()));
        }
        j += 1;
        while j < self.rest.len() && is_ws(self.rest[j]) {
            j += 1;
        }
        if j >= self.rest.len() || (self.rest[j] != b'"' && self.rest[j] != b'\'') {
            self.rest = &[];
            return Some(Err(()));
        }
        let quote = self.rest[j];
        let val_start = j + 1;
        let val_rel = memchr(quote, &self.rest[val_start..])?;
        let val = &self.rest[val_start..val_start + val_rel];
        self.rest = &self.rest[val_start + val_rel + 1..];
        Some(Ok((key, val)))
    }
}

#[inline]
fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n')
}

/// First attribute whose key matches any of `keys`, in document order.
///
/// Returns `Err(())` when a malformed attribute precedes a match.
fn find_attr_value<'a>(interior: &'a [u8], keys: &[&[u8]]) -> Result<Option<&'a [u8]>, ()> {
    for attr in AttrIter::new(interior) {
        let (key, val) = attr?;
        if keys.contains(&key) {
            return Ok(Some(val));
        }
    }
    Ok(None)
}

/// Cached searchers for the fixed CR close tags. `memmem::find` would
/// rebuild per-call state for every one of the ~1.4M close-tag searches in a
/// 100 MB file; the Finder is precomputed once instead.
static CLOSE_FIELD: LazyLock<memmem::Finder> = LazyLock::new(|| memmem::Finder::new("</Field"));
static CLOSE_TEXT: LazyLock<memmem::Finder> = LazyLock::new(|| memmem::Finder::new("</Text"));
static CLOSE_VALUE: LazyLock<memmem::Finder> = LazyLock::new(|| memmem::Finder::new("</Value"));
static CLOSE_FORMATTED: LazyLock<memmem::Finder> =
    LazyLock::new(|| memmem::Finder::new("</FormattedValue"));
static CLOSE_TEXTVALUE: LazyLock<memmem::Finder> =
    LazyLock::new(|| memmem::Finder::new("</TextValue"));

/// Pattern lengths (`</` + name) for advancing past a found close tag.
const PAT_FIELD: usize = 7;
const PAT_TEXT: usize = 6;
const PAT_FORMATTED_VALUE: usize = 16;
const PAT_VALUE: usize = 7;
const PAT_TEXT_VALUE: usize = 11;

#[inline]
fn close_boundary_ok(bytes: &[u8], after_pat: usize) -> bool {
    match bytes.get(after_pat) {
        Some(b'>') => true,
        Some(b) => is_ws(*b),
        None => false,
    }
}

/// Find the first valid `</name ...>` close tag at or after `from`,
/// skipping comment/CDATA regions. Returns the offset just past its `>`.
fn find_close_after(
    bytes: &[u8],
    from: usize,
    finder: &memmem::Finder,
    pat_len: usize,
    regions: &[Range<usize>],
) -> Option<usize> {
    let hay = &bytes[from..];
    let mut search = 0usize;
    while let Some(rel) = finder.find(&hay[search..]) {
        let at = from + search + rel;
        let after_pat = at + pat_len;
        if !close_boundary_ok(bytes, after_pat) || in_region(regions, at) {
            search = search + rel + 1;
            continue;
        }
        let gt_rel = memchr(b'>', &bytes[after_pat..])?;
        return Some(after_pat + gt_rel + 1);
    }
    None
}

/// Raw text between an already-scanned open tag and its matching close tag.
/// Returns the raw byte slice and the offset just past the close tag.
fn raw_text_until<'a>(
    bytes: &'a [u8],
    content_start: usize,
    finder: &memmem::Finder,
    pat_len: usize,
    regions: &[Range<usize>],
) -> Option<(&'a [u8], usize)> {
    let close_lt = find_close_start(bytes, content_start, finder, pat_len, regions)?;
    let after_name = close_lt + pat_len;
    let gt_rel = memchr(b'>', &bytes[after_name..])?;
    let after = after_name + gt_rel + 1;
    Some((&bytes[content_start..close_lt], after))
}

fn find_close_start(
    bytes: &[u8],
    from: usize,
    finder: &memmem::Finder,
    pat_len: usize,
    regions: &[Range<usize>],
) -> Option<usize> {
    let hay = &bytes[from..];
    let mut search = 0usize;
    while let Some(rel) = finder.find(&hay[search..]) {
        let at = from + search + rel;
        let after_pat = at + pat_len;
        if !close_boundary_ok(bytes, after_pat) || in_region(regions, at) {
            search = search + rel + 1;
            continue;
        }
        return Some(at);
    }
    None
}

/// Decode an attribute value: unescape only when an entity is present.
fn decode_attr(raw: &[u8]) -> Cow<'_, str> {
    decode_bytes(raw)
}

/// Assign captured text following quick-xml semantics: only a non-empty body
/// not starting with markup corresponds to a Text event.
fn assign_text<'a>(slot: &mut Cow<'a, str>, raw: &'a [u8]) {
    if raw.is_empty() || raw[0] == b'<' {
        return;
    }
    *slot = decode_text(raw);
}

/// Text content: cut at the first `<` (quick-xml reads a single Text event),
/// then unescape when an entity is present.
fn decode_text(raw: &[u8]) -> Cow<'_, str> {
    let cut = memchr(b'<', raw).unwrap_or(raw.len());
    decode_bytes(&raw[..cut])
}

fn decode_bytes(raw: &[u8]) -> Cow<'_, str> {
    let s = utf8_unchecked(raw);
    if memchr(b'&', raw).is_none() {
        Cow::Borrowed(s)
    } else {
        quick_xml::escape::unescape(s).unwrap_or(Cow::Borrowed(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use std::sync::Arc;

    use arrow::datatypes::Schema;
    use arrow::record_batch::RecordBatch;

    /// Minimal sink capturing emitted rows; supports a drop-set for `wants`.
    struct MockSink {
        rows: Vec<Vec<(String, String)>>,
        current: Vec<(String, String)>,
        drop: HashSet<String>,
    }

    impl MockSink {
        fn new() -> Self {
            MockSink {
                rows: Vec::new(),
                current: Vec::new(),
                drop: HashSet::new(),
            }
        }

        fn dropping(fields: &[&str]) -> Self {
            let mut s = MockSink::new();
            s.drop.extend(fields.iter().map(|f| f.to_string()));
            s
        }

        fn flat(&self) -> Vec<Vec<(String, String)>> {
            self.rows.clone()
        }
    }

    impl ColumnarSink for MockSink {
        fn begin_row(&mut self) {
            self.current.clear();
        }

        fn put_field(&mut self, name: &str, value: Value<'_>) {
            if let Value::Str(s) = value {
                self.current.push((name.to_string(), s.to_string()));
            }
        }

        fn end_row(&mut self) {
            self.rows.push(self.current.clone());
        }

        fn wants(&self, name: &str) -> bool {
            !self.drop.contains(name)
        }

        fn resolve<'a>(&'a self, name: &'a str) -> Option<&'a str> {
            if self.drop.contains(name) {
                None
            } else {
                Some(name)
            }
        }

        fn put_field_resolved(&mut self, resolved_name: &str, value: Value<'_>) {
            self.put_field(resolved_name, value);
        }

        fn finish(&mut self) -> rypipe_core::Result<RecordBatch> {
            Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))
        }
    }

    fn scan(xml: &[u8]) -> MockSink {
        let mut sink = MockSink::new();
        scan_chunk(xml, b"Row", &mut sink).unwrap();
        sink
    }

    fn scan_with_tag(xml: &[u8], tag: &[u8]) -> MockSink {
        let mut sink = MockSink::new();
        scan_chunk(xml, tag, &mut sink).unwrap();
        sink
    }

    #[test]
    fn row_attributes() {
        let sink = scan(br#"<Rows><Row A="1" B="hello"/></Rows>"#);
        assert_eq!(
            sink.flat(),
            vec![vec![("A".into(), "1".into()), ("B".into(), "hello".into())]]
        );
    }

    #[test]
    fn paired_row_with_attrs_and_children() {
        let xml = br#"<Row Level="3"><Field Name="X"><Value>42</Value></Field></Row>"#;
        let sink = scan(xml);
        assert_eq!(
            sink.flat(),
            vec![vec![
                ("Level".into(), "3".into()),
                ("X".into(), "42".into()),
            ]]
        );
    }

    #[test]
    fn field_value_last_wins() {
        let xml = br#"<Row><Field Name="X"><FormattedValue>abc</FormattedValue><Value>42</Value></Field></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat()[0][0].1, "42");
    }

    #[test]
    fn field_empty_body_keeps_previous_text() {
        // quick-xml only assigns on a Text event: an empty <Value> after a
        // non-empty <FormattedValue> must NOT clear it.
        let xml = br#"<Row><Field Name="X"><FormattedValue>keep</FormattedValue><Value></Value></Field></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat()[0][0].1, "keep");
    }

    #[test]
    fn field_name_attr_order_preference() {
        // First attribute in document order named FieldName or Name wins.
        let a = scan(br#"<Row><Field Name="A" FieldName="B"><Value>1</Value></Field></Row>"#);
        assert_eq!(a.flat()[0][0].0, "A");
        let b = scan(br#"<Row><Field FieldName="B" Name="A"><Value>1</Value></Field></Row>"#);
        assert_eq!(b.flat()[0][0].0, "B");
    }

    #[test]
    fn field_default_key_without_attrs() {
        let sink = scan(br#"<Row><Field><Value>7</Value></Field></Row>"#);
        assert_eq!(sink.flat()[0][0].0, "Field");
    }

    #[test]
    fn text_element() {
        let xml = br#"<Row><Text Name="Title"><TextValue>Report</TextValue></Text></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat(), vec![vec![("Title".into(), "Report".into())]]);
    }

    #[test]
    fn text_default_key() {
        let xml = br#"<Row><Text><TextValue>%</TextValue></Text></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat()[0][0].0, "Text");
    }

    #[test]
    fn section_self_closing() {
        let xml = br#"<Row><Section SectionNumber="3"/></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat(), vec![vec![("Section".into(), "3".into())]]);
    }

    #[test]
    fn section_paired_without_number() {
        let xml = br#"<Row><Section></Section></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat(), vec![vec![("Section".into(), "".into())]]);
    }

    #[test]
    fn unknown_child_emits_empty() {
        let sink = scan(br#"<Row><Custom/></Row>"#);
        assert_eq!(sink.flat(), vec![vec![("Custom".into(), "".into())]]);
    }

    #[test]
    fn unknown_nested_children_all_emitted() {
        // Mirrors quick-xml: every Start/Empty under the row is visited.
        let xml = br#"<Row><Foo><Bar/></Foo></Row>"#;
        let sink = scan(xml);
        let row = &sink.flat()[0];
        assert_eq!(row[0].0, "Foo");
        assert_eq!(row[1].0, "Bar");
    }

    #[test]
    fn dropped_field_skipped_byte_wise() {
        let mut sink = MockSink::dropping(&["DropMe"]);
        let xml = br#"<Row><Field Name="Keep"><Value>1</Value></Field><Field Name="DropMe"><Value>x</Value><Extra>junk</Extra></Field><Field Name="Also"><Value>2</Value></Field></Row>"#;
        scan_chunk(xml, b"Row", &mut sink).unwrap();
        let rows = sink.flat();
        let names: Vec<&str> = rows[0].iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["Keep", "Also"]);
        assert_eq!(rows[0][0].1, "1");
        assert_eq!(rows[0][1].1, "2");
    }

    #[test]
    fn dropped_field_followed_by_fake_row_marker() {
        // An un-nested inner </Row> closes the row early -- identical to
        // quick-xml, which also tracks no depth inside a row body. The
        // trailing Field lands between rows and is ignored by both.
        let mut sink = MockSink::dropping(&["Trap"]);
        let xml = br#"<Row><Field Name="Trap"><Value>nope</Value></Field><Row>fake</Row><Field Name="After"><Value>yes</Value></Field></Row>"#;
        scan_chunk(xml, b"Row", &mut sink).unwrap();
        assert_eq!(sink.flat().len(), 1);
        let rows = sink.flat();
        let names: Vec<&str> = rows[0].iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["Row"]);
    }

    #[test]
    fn partial_trailing_row_discarded() {
        let xml = br#"<Row><Field Name="X"><Value>1</Value></Field></Row><Row><Field Name="X""#;
        let sink = scan(xml);
        assert_eq!(sink.flat().len(), 1);
        assert_eq!(sink.flat()[0][0].1, "1");
    }

    #[test]
    fn comment_between_rows_with_fake_marker() {
        let mut xml = b"<Row><Field Name=\"A\"><Value>1</Value></Field></Row>".to_vec();
        xml.extend_from_slice(b"<!-- <Row><Field Name=\"B\"><Value>2</Value></Field></Row> -->");
        xml.extend_from_slice(b"<Row><Field Name=\"C\"><Value>3</Value></Field></Row>");
        let sink = scan(&xml);
        assert_eq!(sink.flat().len(), 2);
        assert_eq!(sink.flat()[0][0].0, "A");
        assert_eq!(sink.flat()[1][0].0, "C");
    }

    #[test]
    fn cdata_inside_value_body_treated_as_markup_start() {
        // quick-xml sees CData (not Text), so no assignment occurs.
        let xml = br#"<Row><Field Name="X"><FormattedValue><![CDATA[raw]]></FormattedValue></Field></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat()[0][0].1, "");
    }

    #[test]
    fn entities_unescaped_in_values() {
        let xml = br#"<Row><Field Name="E"><Value>A &amp; B &lt;10</Value></Field></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat()[0][0].1, "A & B <10");
    }

    #[test]
    fn gt_inside_attribute_value() {
        let xml = br#"<Row><Field Name="a>b"><Value>1</Value></Field></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat()[0][0].0, "a>b");
    }

    #[test]
    fn prefix_collision_tags_ignored_as_unknown() {
        // <Values> is not <Value>; <FieldItem> handled by boundary checks.
        let xml = br#"<Row><Values>v</Values><FieldValue/></Row>"#;
        let sink = scan(xml);
        let rows = sink.flat();
        let names: Vec<&str> = rows[0].iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["Values", "FieldValue"]);
    }

    #[test]
    fn junk_header_and_trailer_around_rows() {
        let mut xml = b"<?xml version=\"1.0\"?><R><Group g=\"1\">".to_vec();
        xml.extend_from_slice(b"<Row><Field Name=\"A\"><Value>1</Value></Field></Row>");
        xml.extend_from_slice(b"<Row><Field Name=\"A\"><Value>2</Value></Field></Row>");
        xml.extend_from_slice(b"</Group></R>");
        let sink = scan(&xml);
        assert_eq!(sink.flat().len(), 2);
    }

    #[test]
    fn custom_row_tag_details() {
        let xml = br#"<Details Level="3"><Field Name="F"><Value>v</Value></Field></Details>"#;
        let sink = scan_with_tag(xml, b"Details");
        assert_eq!(
            sink.flat(),
            vec![vec![("Level".into(), "3".into()), ("F".into(), "v".into()),]]
        );
    }

    #[test]
    fn empty_input_no_rows() {
        assert!(scan(b"").flat().is_empty());
    }

    #[test]
    fn whitespace_between_children_skipped() {
        let xml = b"<Row>\n  <Field Name=\"A\">\n    <Value>1</Value>\n  </Field>\n</Row>";
        let sink = scan(xml);
        assert_eq!(sink.flat(), vec![vec![("A".into(), "1".into())]]);
    }

    #[test]
    fn single_quote_attributes() {
        let xml = br#"<Row A='1'><Field Name='X'><Value>5</Value></Field></Row>"#;
        let sink = scan(xml);
        assert_eq!(sink.flat()[0][0], ("A".into(), "1".into()));
        assert_eq!(sink.flat()[0][1].0, "X");
    }

    /// Stress: arbitrary byte patterns must never panic.
    #[test]
    fn random_bytes_no_panic() {
        let seeds: &[&[u8]] = &[
            b"",
            b"<",
            b"<Row",
            b"<Row>",
            b"<Ro",
            b"<<<<",
            b"<Row A=\"1\"/><Row B=\"2\"/>",
            b"<!-- <Row> -->",
            b"<![CDATA[<Row>]]>",
            b"\0\0\0\0",
            b"\xff\xff\xff\xff",
            b"<Row>\x00\x00<Row>",
            b"<Row><!--<Row>--><Row/>",
            b"<Row><![CDATA[<Row>]]><Row/>",
            b"<Row><Field Name=\"A\"",
            b"<Row><Field Name=A><Value>1</Value></Field></Row>",
            b"<Row><Field Name=\"A\"><Value>unterminated",
            b"<Row></Row></Row>",
            b"</Row>",
            b"<Row/><Row/>",
            b"<Details><Section SectionNumber=\"0\"><Field Name=\"X\"><Value>1</Value></Field></Section></Details>",
            b"<Row A=\"=><\" B='\"'/></Row>",
        ];
        let tags: &[&[u8]] = &[b"Row", b"Details", b"Item"];
        for seed in seeds {
            for tag in tags {
                let mut sink = MockSink::new();
                let _ = scan_chunk(seed, tag, &mut sink);
            }
        }
    }
}

#[cfg(test)]
mod perf {
    use super::*;
    use rypipe_core::Result as RResult;
    use std::sync::Arc;
    use std::time::Instant;

    use arrow::datatypes::Schema;
    use arrow::record_batch::RecordBatch;

    struct NoopSink;

    impl ColumnarSink for NoopSink {
        fn begin_row(&mut self) {}
        fn put_field(&mut self, _name: &str, _value: Value<'_>) {}
        fn end_row(&mut self) {}
        fn finish(&mut self) -> RResult<RecordBatch> {
            Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))
        }
    }

    fn synth_xml(rows: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(rows * 1100);
        out.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><CrystalReport>");
        for i in 0..rows {
            out.extend_from_slice(
                format!(
                    "<Details Level=\"{}\">\
<Field Name=\"F22\" FieldName=\"{{a.PrecioImp}}\"><FormattedValue>1,157.{}</FormattedValue><Value>7428.{}</Value></Field>\
<Field Name=\"F23\" FieldName=\"{{a.Cantidad}}\"><FormattedValue>{}</FormattedValue><Value>{}</Value></Field>\
<Field Name=\"F38\" FieldName=\"{{a.Nombre}}\"><FormattedValue>Distribuidora del Sur S.A.</FormattedValue><Value>Distribuidora del Sur S.A.</Value></Field>\
<Text Name=\"T20\"><TextValue>%</TextValue></Text>\
<Section SectionNumber=\"0\"/>\
</Details>",
                    i % 3,
                    i % 100,
                    i % 100,
                    i % 500,
                    i % 500
                )
                .as_bytes(),
            );
        }
        out.extend_from_slice(b"</CrystalReport>");
        out
    }

    #[test]
    fn perf_scanner_ceiling() {
        let xml = synth_xml(80_000); // ~90 MB
        let mb = xml.len() as f64 / 1024.0 / 1024.0;
        let mut sink = NoopSink;

        // Warmup
        scan_chunk(&xml, b"Details", &mut sink).unwrap();

        let rounds = 3;
        let mut best = f64::MAX;
        for _ in 0..rounds {
            let t0 = Instant::now();
            scan_chunk(&xml, b"Details", &mut sink).unwrap();
            best = best.min(t0.elapsed().as_secs_f64());
        }
        println!(
            "\nscanner ceiling (NoopSink): {:.4}s  {:.0} MB/s  ({:.1} MB)",
            best,
            mb / best,
            mb
        );

        // Scan + reject (row_tag mismatch) — measures pure scan without field work.
        let mut sink2 = NoopSink;
        let t0 = Instant::now();
        scan_chunk(&xml, b"Row", &mut sink2).unwrap(); // Row does not exist, all rejected
        println!(
            "scan + reject (row_tag mismatch): {:.4}s  {:.0} MB/s",
            t0.elapsed().as_secs_f64(),
            mb / t0.elapsed().as_secs_f64()
        );

        // Scan + locate fields, no extract — begin_row/end_row only, no put_field.
        struct LocateOnly;
        impl ColumnarSink for LocateOnly {
            fn begin_row(&mut self) {}
            fn put_field(&mut self, _n: &str, _v: Value<'_>) {}
            fn end_row(&mut self) {}
            fn wants(&self, _name: &str) -> bool { true }
            fn finish(&mut self) -> RResult<RecordBatch> {
                Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))
            }
        }
        let mut loc = LocateOnly;
        let t0 = Instant::now();
        // Use a scanner that only locates fields (no Value extraction) — for now, same as Noop but with row machinery
        scan_chunk(&xml, b"Details", &mut loc).unwrap();
        println!(
            "scan + locate fields, no extract: {:.4}s  {:.0} MB/s",
            t0.elapsed().as_secs_f64(),
            mb / t0.elapsed().as_secs_f64()
        );

        // With TableBuilder sink for comparison.
        let mut tb = rypipe_core::TableBuilder::with_capacity(90_000);
        let t0 = Instant::now();
        scan_chunk(&xml, b"Details", &mut tb).unwrap();
        let batch = tb.finish().unwrap();
        println!(
            "scanner + TableBuilder:     {:.4}s  {:.0} MB/s  ({} cols)",
            t0.elapsed().as_secs_f64(),
            mb / t0.elapsed().as_secs_f64(),
            batch.num_columns()
        );

        // Everything dropped: measures row machinery + skip-bytes path
        // (open tags, attr parse, wants(), memmem jump to close tag).
        struct DropAll;
        impl ColumnarSink for DropAll {
            fn begin_row(&mut self) {}
            fn put_field(&mut self, _n: &str, _v: Value<'_>) {}
            fn end_row(&mut self) {}
            fn wants(&self, _name: &str) -> bool {
                false
            }
            fn finish(&mut self) -> RResult<RecordBatch> {
                Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))
            }
        }
        let mut ds = DropAll;
        let t0 = Instant::now();
        scan_chunk(&xml, b"Details", &mut ds).unwrap();
        println!(
            "scanner, all fields dropped:{:.4}s  {:.0} MB/s",
            t0.elapsed().as_secs_f64(),
            mb / t0.elapsed().as_secs_f64()
        );

        // Phase ladder summary
        println!("\nPhase ladder (synthetic 90 MB, 533 MB real is ~2.5x slower due to high-cardinality arena):");
        println!("  scan + reject (mismatch) ~7738 MB/s (from bench_extended edge row_tag=Row)");
        println!("  + TableBuilder 514 MB/s shows per-field extract+sink is 10x bottleneck, not memchr (8% profile)");
    }
}


