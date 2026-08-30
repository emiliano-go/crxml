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

#[cfg(feature = "profile")]
use std::sync::atomic::{AtomicU64, Ordering};

use memchr::{memchr, memchr3, memmem};

use rypipe_core::{ColumnarSink, Value};

use crate::xml::splitter::{find_special_regions, next_row_start};

// --- Predicate-first profiling counters ---
// Counts rows where row_rejected() fired and find_close_details skipped the rest.
#[cfg(feature = "profile")]
pub static REJECTED_ROWS: AtomicU64 = AtomicU64::new(0);
// Counts individual fields that were skipped (i.e. never visited) because the
// row was already rejected. Measured by counting <Field occurrences between the
// current scan position and the </Details> close tag.
#[cfg(feature = "profile")]
pub static SKIPPED_FIELDS: AtomicU64 = AtomicU64::new(0);
// Counts fields where the predicate was evaluated (incremented on each
// evaluate_predicate_state call).
#[cfg(feature = "profile")]
pub static PREDICATE_CHECKS: AtomicU64 = AtomicU64::new(0);
// Counts total rows scanned (incremented at start of each parse_row).
#[cfg(feature = "profile")]
pub static ROWS_SCANNED: AtomicU64 = AtomicU64::new(0);
// Counts how many times row_rejected() is called (before the if-check).
#[cfg(feature = "profile")]
pub static REJECTED_CHECKS: AtomicU64 = AtomicU64::new(0);

/// Reset all profiling counters (for benchmark isolation).
#[cfg(feature = "profile")]
pub fn reset_profile_counters() {
    REJECTED_ROWS.store(0, Ordering::Relaxed);
    SKIPPED_FIELDS.store(0, Ordering::Relaxed);
    PREDICATE_CHECKS.store(0, Ordering::Relaxed);
    ROWS_SCANNED.store(0, Ordering::Relaxed);
    REJECTED_CHECKS.store(0, Ordering::Relaxed);
    rypipe_core::PREDICATE_EVALUATIONS.store(0, Ordering::Relaxed);
    rypipe_core::PREDICATE_FAILS.store(0, Ordering::Relaxed);
    rypipe_core::PREDICATE_UNDECIDED.store(0, Ordering::Relaxed);
    rypipe_core::IS_PRED_TRUE.store(0, Ordering::Relaxed);
    rypipe_core::IS_PRED_FALSE.store(0, Ordering::Relaxed);
    rypipe_core::RESOLVE_AND_PUT_COUNT.store(0, Ordering::Relaxed);
}

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
pub fn scan_chunk<S: ColumnarSink + ?Sized>(
    bytes: &[u8],
    row_tag: &[u8],
    sink: &mut S,
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
pub(crate) fn scan_one_row<S: ColumnarSink + ?Sized>(
    bytes: &[u8],
    mut pos: usize,
    row_tag: &[u8],
    regions: &[Range<usize>],
    sink: &mut S,
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
fn parse_row<S: ColumnarSink + ?Sized>(
    bytes: &[u8],
    lt: usize,
    row_tag: &[u8],
    regions: &[Range<usize>],
    sink: &mut S,
) -> Flow {
    let open = match scan_open_tag(bytes, lt) {
        Some(o) => o,
        None => return Flow::Truncated,
    };

    #[cfg(feature = "profile")]
    ROWS_SCANNED.fetch_add(1, Ordering::Relaxed);

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
        #[cfg(feature = "profile")]
        REJECTED_CHECKS.fetch_add(1, Ordering::Relaxed);
        if sink.row_rejected() {
            #[cfg(feature = "profile")]
            {
                REJECTED_ROWS.fetch_add(1, Ordering::Relaxed);
            }
            let after = find_close_details(bytes, cur, regions);
            if let Some(after) = after {
                #[cfg(feature = "profile")]
                {
                    let skipped = count_field_openings(&bytes[cur..after.min(bytes.len())]);
                    SKIPPED_FIELDS.fetch_add(skipped, Ordering::Relaxed);
                }
                sink.end_row();
                return Flow::At(after);
            } else {
                return Flow::Truncated;
            }
        }
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

fn scan_child<S: ColumnarSink + ?Sized>(
    bytes: &[u8],
    cur: usize,
    row_tag: &[u8],
    regions: &[Range<usize>],
    sink: &mut S,
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
            sink.resolve_and_put(name, Value::Str(Cow::Borrowed("")));
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
fn emit_all_attrs<S: ColumnarSink + ?Sized>(bytes: &[u8], open: &OpenTag<'_>, sink: &mut S) -> bool {
    for attr in AttrIter::new(open.interior(bytes)) {
        match attr {
            Ok((key_raw, val_raw)) => {
                let key = decode_attr(key_raw);
                let val = decode_attr(val_raw);
                sink.resolve_and_put(&key, Value::Str(val));
            }
            Err(()) => return true,
        }
    }
    false
}

/// Handle one `<Field ...>` element: skip entirely when the plan drops the
/// resolved column, otherwise capture FormattedValue/Value text (last wins).
fn field_element<'a, S: ColumnarSink + ?Sized>(
    bytes: &'a [u8],
    open: &OpenTag<'_>,
    regions: &[Range<usize>],
    sink: &mut S,
) -> Flow {
    let interior = open.interior(bytes);
    let key_raw = match find_attr_value(interior, &[b"FieldName", b"Name"]) {
        Ok(Some(v)) => v,
        Ok(None) => b"Field",
        Err(()) => return Flow::Recover,
    };
    let key = decode_attr(key_raw);

    // Fast paths for locate-only and traversal-only tiers.
    if !sink.needs_value() {
        if !sink.needs_resolve() {
            // Traverse-only: find field extents, no sink calls at all.
            return match find_close_after(bytes, open.after(), &CLOSE_FIELD, PAT_FIELD, regions) {
                Some(after) => Flow::At(after),
                None => Flow::Truncated,
            };
        }
        // Locate-only: resolve field name, skip text extraction.
        // wants() + resolve() are called; put_field is NOT.
        if sink.wants(&key) {
            let _resolved = sink.resolve(&key);
        }
        return match find_close_after(bytes, open.after(), &CLOSE_FIELD, PAT_FIELD, regions) {
            Some(after) => Flow::At(after),
            None => Flow::Truncated,
        };
    }

    // Check if field is dropped via resolve (single hash probe).
    // If kept, extract text and push via resolve_and_put (no second probe, no allocation).
    let kept = sink.resolve(&key).is_some();
    if !kept {
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
                sink.resolve_and_put(&key, Value::Str(text));
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
fn text_element<'a, S: ColumnarSink + ?Sized>(
    bytes: &'a [u8],
    open: &OpenTag<'_>,
    regions: &[Range<usize>],
    sink: &mut S,
) -> Flow {
    let interior = open.interior(bytes);
    let key_raw = match find_attr_value(interior, &[b"Name"]) {
        Ok(Some(v)) => v,
        Ok(None) => b"Text",
        Err(()) => return Flow::Recover,
    };
    let key = decode_attr(key_raw);

    // Fast paths for locate-only and traversal-only tiers.
    if !sink.needs_value() {
        if !sink.needs_resolve() {
            // Traverse-only: find field extents, no sink calls at all.
            return match find_close_after(bytes, open.after(), &CLOSE_TEXT, PAT_TEXT, regions) {
                Some(after) => Flow::At(after),
                None => Flow::Truncated,
            };
        }
        // Locate-only: resolve field name, skip text extraction.
        if sink.wants(&key) {
            let _resolved = sink.resolve(&key);
        }
        return match find_close_after(bytes, open.after(), &CLOSE_TEXT, PAT_TEXT, regions) {
            Some(after) => Flow::At(after),
            None => Flow::Truncated,
        };
    }

    // Check if field is dropped via resolve (single hash probe).
    let kept = sink.resolve(&key).is_some();
    if !kept {
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
                sink.resolve_and_put(&key, Value::Str(text));
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
fn section_element<S: ColumnarSink + ?Sized>(bytes: &[u8], open: &OpenTag<'_>, sink: &mut S) -> Flow {
    let interior = open.interior(bytes);
    let sn = match find_attr_value(interior, &[b"SectionNumber"]) {
        Ok(Some(v)) => decode_attr(v),
        Ok(None) => Cow::Borrowed(""),
        Err(()) => return Flow::Recover,
    };
    sink.resolve_and_put("Section", Value::Str(sn));
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

/// Byte search: delegates to memchr (AVX2/SSE2).
/// memchr handles short haystacks internally via its own thresholds.
#[inline(always)]
fn scan_byte(haystack: &[u8], needle: u8) -> Option<usize> {
    memchr(needle, haystack)
}

/// Offset of the next `<` at or after `from`.
#[inline(always)]
fn next_lt(bytes: &[u8], from: usize) -> Option<usize> {
    scan_byte(&bytes[from..], b'<').map(|rel| from + rel)
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
#[inline(always)]
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

    // Walk to '>' honoring quoted attribute values.
    // Scalar triple-scan for short interiors, memchr3 for long.
    let gt = loop {
        let remaining = &bytes[i..];
        let rel = memchr3(b'"', b'\'', b'>', remaining)?;
        let at = i + rel;
        if bytes[at] == b'>' {
            break at;
        }
        // Jump past the matching closing quote in one search.
        i = at + 1 + scan_byte(&bytes[at + 1..], bytes[at])? + 1;
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
#[inline(always)]
fn scan_close_tag(bytes: &[u8], lt: usize) -> Option<(&[u8], usize)> {
    let start = lt + 2;
    let gt_rel = scan_byte(&bytes[start..], b'>')?;
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
        let end_rel = COMMENT_END.find(&bytes[lt + 4..])?;
        return Some(lt + 4 + end_rel + 3);
    }
    if rest.starts_with(b"<![CDATA[") {
        let end_rel = CDATA_END.find(&bytes[lt + 9..])?;
        return Some(lt + 9 + end_rel + 3);
    }
    // PI, DOCTYPE, and friends end at the first '>'.
    let gt_rel = scan_byte(&bytes[lt + 1..], b'>')?;
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
#[inline(always)]
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
static CLOSE_DETAILS: LazyLock<memmem::Finder> = LazyLock::new(|| memmem::Finder::new("</Details"));

// Searchers for skip_construct (comments / CDATA end markers).
static COMMENT_END: LazyLock<memmem::Finder> =
    LazyLock::new(|| memmem::Finder::new("-->"));
static CDATA_END: LazyLock<memmem::Finder> =
    LazyLock::new(|| memmem::Finder::new("]]>"));

// Searchers for estimate_bytes_per_row (row-tag prefix).
static DETAILS_OPEN: LazyLock<memmem::Finder> =
    LazyLock::new(|| memmem::Finder::new("<Details"));

/// Pattern lengths (`</` + name) for advancing past a found close tag.
const PAT_FIELD: usize = 7;
const PAT_TEXT: usize = 6;
const PAT_DETAILS: usize = 9;
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
        let gt_rel = scan_byte(&bytes[after_pat..], b'>')?;
        return Some(after_pat + gt_rel + 1);
    }
    None
}

fn find_close_details(bytes: &[u8], from: usize, regions: &[Range<usize>]) -> Option<usize> {
    find_close_after(bytes, from, &CLOSE_DETAILS, PAT_DETAILS, regions)
}

/// Count `<Field` occurrences in a byte slice (for profiling: how many fields
/// were skipped when a row was rejected).
#[cfg(feature = "profile")]
fn count_field_openings(region: &[u8]) -> u64 {
    static FIELD_OPEN: LazyLock<memmem::Finder<'static>> =
        LazyLock::new(|| memmem::Finder::new(b"<Field"));
    let mut count = 0u64;
    let mut pos = 0;
    while pos < region.len() {
        if let Some(rel) = FIELD_OPEN.find(&region[pos..]) {
            count += 1;
            pos += rel + 6; // len("<Field")
        } else {
            break;
        }
    }
    count
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
    let gt_rel = scan_byte(&bytes[after_name..], b'>')?;
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
    let cut = scan_byte(raw, b'<').unwrap_or(raw.len());
    decode_bytes(&raw[..cut])
}

fn decode_bytes(raw: &[u8]) -> Cow<'_, str> {
    let s = utf8_unchecked(raw);
    if scan_byte(raw, b'&').is_none() {
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

    /// Regression: PAT_DETAILS was 10 instead of 9, causing
    /// find_close_details to check the byte PAST the '>' and fail.
    /// This test ensures the close tag is found on well-formed data.
    #[test]
    fn regression_pat_details_close_tag_found() {
        let xml = b"<Row><Details><Field Name=\"A\"><Value>1</Value></Field></Details></Row>";
        let regions = &[];
        // find_close_details should find the end of </Details>
        let result = find_close_details(xml, 0, regions);
        assert!(result.is_some(), "find_close_details must find </Details> on well-formed XML");
        let pos = result.unwrap();
        // After </Details> the remaining bytes should be </Row>
        assert_eq!(&xml[pos..], b"</Row>", "cursor should be right after </Details>");
    }

    /// Same regression check: ensure close_boundary_ok is called at the
    /// right byte — the '>' of '</Details>', not the byte past it.
    #[test]
    fn regression_pat_details_boundary_check_position() {
        // Minimal case: </Details> immediately followed by EOF
        let xml = b"</Details>";
        let result = find_close_details(xml, 0, &[]);
        assert!(result.is_some(), "</Details> at EOF must be found");
        assert_eq!(result.unwrap(), xml.len(), "cursor should be at EOF");
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
        let xml = synth_xml(80_000); // ~90 MB, ~80k rows × 5 fields = ~400k fields
        let mb = xml.len() as f64 / 1024.0 / 1024.0;
        let fields_total: usize = 80_000 * 5; // 5 fields per row (3 Field + 1 Text + 1 Level attr)
        let bytes_per_field = xml.len() as f64 / fields_total as f64;

        // Warmup ALL tiers before timing to avoid cold-start ordering effects.
        let mut ws1 = NoopSink;
        scan_chunk(&xml, b"Details", &mut ws1).unwrap();
        // Warm scan_only (memmem)
        let _ = memchr::memmem::find(&xml, b"<Details");
        // Warm locate tier
        struct WarmLocate;
        impl ColumnarSink for WarmLocate {
            #[inline] fn begin_row(&mut self) {}
            #[inline] fn put_field(&mut self, _n: &str, _v: Value<'_>) {}
            #[inline] fn end_row(&mut self) {}
            #[inline] fn wants(&self, _name: &str) -> bool { true }
            #[inline] fn needs_value(&self) -> bool { false }
            fn finish(&mut self) -> RResult<RecordBatch> {
                Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))
            }
        }
        let mut ww = WarmLocate;
        scan_chunk(&xml, b"Details", &mut ww).unwrap();
        // Warm full tier
        let mut wf = rypipe_core::TableBuilder::with_capacity(90_000);
        scan_chunk(&xml, b"Details", &mut wf).unwrap();
        let _ = wf.finish();

        println!("\n=== Four-tier scanner decomposition ({:.1} MB, {} fields, {:.0} bytes/field) ===", mb, fields_total, bytes_per_field);

        // --- Tier 1: scan_only ---
        // Pure memmem find of row open tags. No XML parsing, no field walking.
        // This is the absolute minimum: how fast can we find row boundaries?
        let t0 = Instant::now();
        let mut count = 0;
        let mut pos = 0;
        while let Some(rel) = memchr::memmem::find(&xml[pos..], b"<Details") {
            count += 1;
            pos += rel + 8; // len("<Details")
        }
        let t_scan = t0.elapsed().as_secs_f64();
        let scan_mbs = mb / t_scan;
        println!("scan_only          {t_scan:8.4}s  {scan_mbs:8.0} MB/s  (memmem <Details> only, {count} rows found)");

        // --- Tier 2: traverse ---
        // Walk fields, find extents, no resolve, no put_field.
        // needs_value()=false + needs_resolve()=false: scanner finds Field/Text elements,
        // reads Name attribute (for traversal cost), but skips wants/resolve/put_field.
        struct TraverseOnly;
        impl ColumnarSink for TraverseOnly {
            #[inline] fn begin_row(&mut self) {}
            #[inline] fn put_field(&mut self, _n: &str, _v: Value<'_>) {}
            #[inline] fn end_row(&mut self) {}
            #[inline] fn wants(&self, _name: &str) -> bool { true }
            #[inline] fn needs_value(&self) -> bool { false }
            #[inline] fn needs_resolve(&self) -> bool { false }
            fn finish(&mut self) -> RResult<RecordBatch> {
                Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))
            }
        }
        let mut trav = TraverseOnly;
        let t0 = Instant::now();
        scan_chunk(&xml, b"Details", &mut trav).unwrap();
        let t_trav = t0.elapsed().as_secs_f64();
        let trav_mbs = mb / t_trav;
        println!("traverse           {t_trav:8.4}s  {trav_mbs:8.0} MB/s  (no resolve, no put_field)");

        // --- Tier 3: locate ---
        // wants() + resolve(), no put_field, no text extraction.
        // needs_value()=false + needs_resolve()=true: scanner calls wants() + resolve()
        // but does NOT call put_field. Uses the SAME resolve path as TableBuilder
        // (via ExecutionPlan) so the cost is comparable.
        struct LocateOnly {
            plan: rypipe_core::ExecutionPlan,
        }
        impl ColumnarSink for LocateOnly {
            #[inline] fn begin_row(&mut self) {}
            #[inline] fn put_field(&mut self, _n: &str, _v: Value<'_>) {}
            #[inline] fn end_row(&mut self) {}
            #[inline] fn wants(&self, _name: &str) -> bool { true }
            #[inline] fn needs_value(&self) -> bool { false }
            #[inline] fn resolve<'a>(&'a self, name: &'a str) -> Option<&'a str> {
                self.plan.resolve_field(name)
            }
            fn finish(&mut self) -> RResult<RecordBatch> {
                Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))
            }
        }
        let mut loc = LocateOnly { plan: rypipe_core::ExecutionPlan::new() };
        let t0 = Instant::now();
        scan_chunk(&xml, b"Details", &mut loc).unwrap();
        let t_loc = t0.elapsed().as_secs_f64();
        let loc_mbs = mb / t_loc;
        println!("locate             {t_loc:8.4}s  {loc_mbs:8.0} MB/s  (wants+resolve, no put_field)");

        // --- Tier 4: full ---
        // Everything: scan + locate + extract + sink.
        let mut tb = rypipe_core::TableBuilder::with_capacity(90_000);
        let t0 = Instant::now();
        scan_chunk(&xml, b"Details", &mut tb).unwrap();
        let batch = tb.finish().unwrap();
        let t_full = t0.elapsed().as_secs_f64();
        let full_mbs = mb / t_full;
        let rows = batch.num_rows();
        println!("full_parse         {t_full:8.4}s  {full_mbs:8.0} MB/s  ({} rows, {} cols)", rows, batch.num_columns());

        // --- Cross-tier assertions ---
        assert!(rows > 0, "full_parse produced zero rows");
        assert!(batch.num_columns() > 0, "full_parse produced zero columns");
        assert_eq!(count, rows, "scan_only found {count} rows but full_parse found {rows} — tiers are inconsistent");

        // --- ms/MB decomposition (additive, not ratios) ---
        println!("\nms/MB decomposition (additive, each rung adds exactly one cost layer):");
        println!("  scan_only:    {:.2} ms/MB", t_scan / mb * 1000.0);
        println!("  traverse:     {:.2} ms/MB  (+{:.2} = XML tree walk + field extent scan)", t_trav / mb * 1000.0, (t_trav - t_scan) / mb * 1000.0);
        println!("  locate:       {:.2} ms/MB  (+{:.2} = field-name resolution)", t_loc / mb * 1000.0, (t_loc - t_trav) / mb * 1000.0);
        println!("  full_parse:   {:.2} ms/MB  (+{:.2} = value extraction + Arrow sink)", t_full / mb * 1000.0, (t_full - t_loc) / mb * 1000.0);
        println!("  total:        {:.2} ms/MB", t_full / mb * 1000.0);
    }

    /// Run six-tier decomposition on a real file using mmap (same path as
    /// production `read_to_columnar`). Median-of-7 with adaptive CoV reporting.
    ///
    /// Six tiers: scan_only → traverse → locate → push_only → build_only → full_parse.
    /// Each tier adds exactly one cost layer (additive, not ratios).
    ///
    /// Set BENCH_FILE env var to the XML file path, e.g.:
    /// ```
    /// BENCH_FILE=bench_data/test_10mb.xml cargo test --release perf_scanner_file -- --nocapture
    /// ```
    #[test]
    fn perf_scanner_file() {
        let path = match std::env::var("BENCH_FILE") {
            Ok(p) => p,
            Err(_) => {
                let candidate = "bench_data/test_10mb.xml";
                if std::path::Path::new(candidate).exists() {
                    candidate.to_string()
                } else {
                    eprintln!("perf_scanner_file: BENCH_FILE not set and {candidate} not found, skipping");
                    return;
                }
            }
        };
        let bytes = std::fs::read(&path).expect("failed to read BENCH_FILE");
        let mb = bytes.len() as f64 / 1024.0 / 1024.0;
        let n = 7usize; // median-of-7

        // BENCH_TIER env var: run only the named tier (for isolated perf stat).
        // Empty or unset = run all tiers.
        let bench_tier = std::env::var("BENCH_TIER").unwrap_or_default();

        // Warmup all tiers (skip when running single tier for perf stat)
        if bench_tier.is_empty() {
            {
                let mut wf = rypipe_core::TableBuilder::with_capacity(100_000);
                scan_chunk(&bytes, b"Details", &mut wf).unwrap();
                let _ = wf.finish();
            }
            {
                let mut wt = TravFile;
                scan_chunk(&bytes, b"Details", &mut wt).unwrap();
            }
            {
                let mut wl = LocateFile { plan: rypipe_core::ExecutionPlan::new() };
                scan_chunk(&bytes, b"Details", &mut wl).unwrap();
            }
        }
        let run = |name: &str| bench_tier.is_empty() || bench_tier == name;

        println!("\n=== Six-tier scanner decomposition ({:.1} MB, {} runs, path: {}) ===", mb, n, path);
        if !bench_tier.is_empty() {
            println!("  (BENCH_TIER={bench_tier} — only running that tier)");
        }

        // Helper: run a closure n times, return (median, CoV)
        fn bench(n: usize, mut f: impl FnMut()) -> (f64, f64) {
            let mut ts: Vec<f64> = (0..n).map(|_| {
                let t0 = Instant::now();
                f();
                t0.elapsed().as_secs_f64()
            }).collect();
            ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let med = ts[n / 2];
            let mean = ts.iter().sum::<f64>() / n as f64;
            let var = ts.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / n as f64;
            let cov = var.sqrt() / mean;
            (med, cov)
        }

        // --- Tier 1: scan_only ---
        let (t_scan, cov_scan) = if run("scan") { bench(n, || {
            let mut _pos = 0usize;
            let mut _count = 0usize;
            while let Some(rel) = memchr::memmem::find(&bytes[_pos..], b"<Details") {
                _count += 1;
                _pos += rel + 8;
            }
        }) } else { (0.0, 0.0) };
        // Count rows once (stable)
        let mut count = 0usize;
        let mut pos = 0;
        while let Some(rel) = memchr::memmem::find(&bytes[pos..], b"<Details") {
            count += 1;
            pos += rel + 8;
        }

        // --- Tier 2: traverse ---
        let (t_trav, cov_trav) = if run("traverse") { bench(n, || {
            let mut trav = TravFile;
            scan_chunk(&bytes, b"Details", &mut trav).unwrap();
        }) } else { (0.0, 0.0) };

        // --- Tier 3: locate ---
        let (t_loc, cov_loc) = if run("locate") { bench(n, || {
            let mut loc = LocateFile { plan: rypipe_core::ExecutionPlan::new() };
            scan_chunk(&bytes, b"Details", &mut loc).unwrap();
        }) } else { (0.0, 0.0) };

        // --- Tier 4: push_only (scan + per-field push, no finish_row) ---
        // end_row calls advance_row() only — skips null-fill, dirty-mask clear,
        // and filter check.  This isolates per-field push cost from per-row
        // finalization cost.
        struct PushOnly {
            inner: rypipe_core::TableBuilder,
        }
        impl ColumnarSink for PushOnly {
            #[inline] fn begin_row(&mut self) {}
            #[inline] fn put_field(&mut self, name: &str, value: Value<'_>) {
                self.inner.put_field(name, value);
            }
            #[inline] fn end_row(&mut self) {
                // Only advance row counter — skip null-fill, dirty mask, filter
                self.inner.advance_row();
            }
            #[inline] fn wants(&self, name: &str) -> bool { self.inner.wants(name) }
            #[inline] fn resolve<'a>(&'a self, name: &'a str) -> Option<&'a str> {
                self.inner.resolve(name)
            }
            #[inline] fn put_field_resolved(&mut self, name: &str, value: Value<'_>) {
                self.inner.put_field_resolved(name, value);
            }
            #[inline] fn resolve_and_put(&mut self, name: &str, value: Value<'_>) {
                self.inner.resolve_and_put(name, value);
            }
            #[inline] fn needs_value(&self) -> bool { true }
            fn finish(&mut self) -> RResult<RecordBatch> {
                self.inner.finish()
            }
        }
        let (t_push, cov_push) = if run("push") { bench(n, || {
            let mut pb = PushOnly { inner: rypipe_core::TableBuilder::with_capacity(100_000) };
            scan_chunk(&bytes, b"Details", &mut pb).unwrap();
        }) } else { (0.0, 0.0) };

        // --- Tier 5: build_only (scan + sink, no Arrow export) ---
        let (t_build, cov_build) = if run("build") { bench(n, || {
            let mut tb = rypipe_core::TableBuilder::with_capacity(100_000);
            scan_chunk(&bytes, b"Details", &mut tb).unwrap();
            // Do NOT call tb.finish() — this isolates scan+push from Arrow export.
        }) } else { (0.0, 0.0) };

        // --- Tier 6: full_parse (scan + sink + Arrow export) ---
        let mut last_rows = 0usize;
        let mut last_cols = 0usize;
        let (t_full, cov_full) = if run("full") { bench(n, || {
            let mut tb = rypipe_core::TableBuilder::with_capacity(100_000);
            scan_chunk(&bytes, b"Details", &mut tb).unwrap();
            let batch = tb.finish().unwrap();
            last_rows = batch.num_rows();
            last_cols = batch.num_columns();
        }) } else { (0.0, 0.0) };

        // --- Cross-tier assertions (skip when running single tier) ---
        if bench_tier.is_empty() {
            assert!(last_rows > 0, "full_parse produced zero rows");
            assert!(last_cols > 0, "full_parse produced zero columns");
            assert_eq!(count, last_rows, "scan_only {count} != full_parse {last_rows}");
        }

        // --- Noise floor: 1.31 × CoV ---
        fn floor(cov: f64) -> f64 { 1.31 * cov * 100.0 }

        // --- Print results ---
        println!("\n{:14} {:>8} {:>8} {:>7} {:>7} {:>8}", "Tier", "Time(s)", "MB/s", "ms/MB", "CoV%", "Floor%");
        println!("{:-<62}", "");
        let mut prev = 0.0f64;
        macro_rules! row {
            ($name:expr, $t:expr, $cov:expr) => {{
                let ms_mb = $t / mb * 1000.0;
                let delta = ms_mb - prev;
                let cov_pct = $cov * 100.0;
                let fl = floor($cov);
                println!("{:14} {:8.4} {:8.0} {:6.2}+{:5.2} {:6.1}% {:6.1}%", $name, $t, mb/$t, ms_mb, delta, cov_pct, fl);
                prev = ms_mb;
            }};
        }
        row!("scan_only", t_scan, cov_scan);
        row!("traverse", t_trav, cov_trav);
        row!("locate", t_loc, cov_loc);
        row!("push_only", t_push, cov_push);
        row!("build_only", t_build, cov_build);
        row!("full_parse", t_full, cov_full);

        // Sanity: deltas must sum to total (skip when running single tier)
        if bench_tier.is_empty() {
            let delta_sum = (t_scan + (t_trav - t_scan) + (t_loc - t_trav)
                + (t_push - t_loc) + (t_build - t_push) + (t_full - t_build)) / mb * 1000.0;
            let total_ms = t_full / mb * 1000.0;
            assert!((delta_sum - total_ms).abs() < 0.001,
                "ladder reconciliation failed: deltas sum to {delta_sum:.3} but total is {total_ms:.3}");
        }

        println!("\nms/MB decomposition (deltas from consecutive tiers):");
        println!("  scan_only:    {:.3} ± {:.1}% ms/MB", t_scan / mb * 1000.0, cov_scan * 100.0);
        println!("  traverse:     {:.3} ± {:.1}% ms/MB  (+{:.3})", t_trav / mb * 1000.0, cov_trav * 100.0, (t_trav - t_scan) / mb * 1000.0);
        println!("  locate:       {:.3} ± {:.1}% ms/MB  (+{:.3})", t_loc / mb * 1000.0, cov_loc * 100.0, (t_loc - t_trav) / mb * 1000.0);
        println!("  push_only:    {:.3} ± {:.1}% ms/MB  (+{:.3})", t_push / mb * 1000.0, cov_push * 100.0, (t_push - t_loc) / mb * 1000.0);
        println!("  build_only:   {:.3} ± {:.1}% ms/MB  (+{:.3})", t_build / mb * 1000.0, cov_build * 100.0, (t_build - t_push) / mb * 1000.0);
        println!("  full_parse:   {:.3} ± {:.1}% ms/MB  (+{:.3})", t_full / mb * 1000.0, cov_full * 100.0, (t_full - t_build) / mb * 1000.0);
        println!("  total:        {:.3} ms/MB", t_full / mb * 1000.0);
        println!("  rows={} cols={}", last_rows, last_cols);
    }

    /// Diagnostic: print per-column memory stats after a push_only run.
    /// Answers: is StrColumn realloc the source of the 250 unaccounted cycles/field?
    #[test]
    fn perf_push_diagnostics() {
        let path = match std::env::var("BENCH_FILE") {
            Ok(p) => p,
            Err(_) => {
                let candidate = "bench_data/test_1gb.xml";
                if std::path::Path::new(candidate).exists() {
                    candidate.to_string()
                } else {
                    eprintln!("perf_push_diagnostics: BENCH_FILE not set and {candidate} not found, skipping");
                    return;
                }
            }
        };
        let bytes = std::fs::read(&path).expect("failed to read BENCH_FILE");
        let mb = bytes.len() as f64 / 1024.0 / 1024.0;

        // Run a single push_only pass and inspect the TableBuilder
        struct PushDiag {
            inner: rypipe_core::TableBuilder,
        }
        impl ColumnarSink for PushDiag {
            #[inline] fn begin_row(&mut self) {}
            #[inline] fn put_field(&mut self, name: &str, value: Value<'_>) {
                self.inner.put_field(name, value);
            }
            #[inline] fn end_row(&mut self) {
                self.inner.advance_row();
            }
            #[inline] fn wants(&self, name: &str) -> bool { self.inner.wants(name) }
            #[inline] fn resolve<'a>(&'a self, name: &'a str) -> Option<&'a str> {
                self.inner.resolve(name)
            }
            #[inline] fn put_field_resolved(&mut self, name: &str, value: Value<'_>) {
                self.inner.put_field_resolved(name, value);
            }
            #[inline] fn resolve_and_put(&mut self, name: &str, value: Value<'_>) {
                self.inner.resolve_and_put(name, value);
            }
            #[inline] fn needs_value(&self) -> bool { true }
            fn finish(&mut self) -> RResult<RecordBatch> {
                self.inner.finish()
            }
        }

        // Use the production estimate: count <Details in 64 KB sample, then derive row count
        let sample_end = bytes.len().min(65536);
        let prefix = b"<Details";
        let row_count = memchr::memmem::find_iter(&bytes[..sample_end], prefix.as_slice()).count();
        let bytes_per_row = if row_count > 0 { sample_end / row_count } else { 512 };
        let est = (bytes.len() / bytes_per_row.max(1)).max(64);
        println!("\n=== Push diagnostics ({:.1} MB, est_rows={est}, bytes_per_row={bytes_per_row}) ===", mb);
        let mut sink = PushDiag {
            inner: rypipe_core::TableBuilder::with_capacity(est.max(64)),
        };
        let t0 = Instant::now();
        scan_chunk(&bytes, b"Details", &mut sink).unwrap();
        let elapsed = t0.elapsed().as_secs_f64();
        println!("push_only (cap={est}): {:.4}s  {:.0} MB/s", elapsed, mb / elapsed);

        // Inspect columns
        let diag = sink.inner.column_diagnostics();
        let actual_rows = sink.inner.num_rows();
        println!("\nactual_rows={actual_rows}, est_rows={est}");
        println!("\n{:>20} {:>12} {:>12} {:>8} {:>8} {:>12}",
            "column", "bytes_used", "bytes_cap", "util%", "rows", "b/row");
        println!("{}", "-".repeat(80));
        let mut total_used = 0usize;
        let mut total_cap = 0usize;
        for (name, used, cap) in diag.iter() {
            let util = if *cap > 0 { *used as f64 / *cap as f64 * 100.0 } else { 0.0 };
            let bpr = if actual_rows > 0 { *used / actual_rows } else { 0 };
            println!("{name:>20} {used:>12} {cap:>12} {util:>7.1}% {actual_rows:>8} {bpr:>12}");
            total_used += used;
            total_cap += cap;
        }
        let total_util = if total_cap > 0 { total_used as f64 / total_cap as f64 * 100.0 } else { 0.0 };
        println!("{:>20} {:>12} {:>12} {:>7.1}%", "TOTAL", total_used, total_cap, total_util);
        println!("\noverhead: {:.1} MB allocated but unused", (total_cap - total_used) as f64 / 1048576.0);

        // Estimate realloc count per column:
        // StrColumn::with_capacity(n) allocates data = n*16 bytes.
        // If actual > capacity, Vec doubles until it fits.
        // Number of reallocs = ceil(log2(actual / capacity))
        println!("\nEstimated reallocs per column (data only, cap * 16 heuristic):");
        for (name, used, _cap) in diag.iter() {
            let initial_data_cap = est * 16; // StrColumn::with_capacity(est).data capacity
            let actual_data = *used; // approx: most bytes_used is data
            if actual_data > initial_data_cap {
                let ratio = actual_data as f64 / initial_data_cap as f64;
                let reallocs = ratio.log2().ceil() as usize;
                let total_copied: u64 = (0..reallocs).map(|i| (initial_data_cap as u64) << i).sum();
                println!("  {name:>20}: cap={initial_data_cap:>10}, used={actual_data:>10}, ratio={ratio:.1}x, ~{reallocs} reallocs, ~{:.1} MB copied",
                    total_copied as f64 / 1048576.0);
            } else {
                println!("  {name:>20}: cap={initial_data_cap:>10}, used={actual_data:>10}, no realloc needed");
            }
        }
    }

    /// Column scaling test: push_only with 1, 3, and 10 columns.
    /// If per-field push cost scales with column count, interleaving is the cause.
    /// If it stays flat, the 261 cycles/field is per-field work regardless of columns.
    #[test]
    fn perf_column_scaling() {
        let path = match std::env::var("BENCH_FILE") {
            Ok(p) => p,
            Err(_) => {
                let candidate = "bench_data/test_100mb.xml";
                if std::path::Path::new(candidate).exists() {
                    candidate.to_string()
                } else {
                    eprintln!("perf_column_scaling: BENCH_FILE not set and {candidate} not found, skipping");
                    return;
                }
            }
        };
        let bytes = std::fs::read(&path).expect("failed to read BENCH_FILE");
        let mb = bytes.len() as f64 / 1024.0 / 1024.0;
        let n = 7usize;

        // Helper: run a closure n times, return (median, CoV)
        fn bench(n: usize, mut f: impl FnMut()) -> (f64, f64) {
            let mut ts: Vec<f64> = (0..n).map(|_| {
                let t0 = Instant::now();
                f();
                t0.elapsed().as_secs_f64()
            }).collect();
            ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let med = ts[n / 2];
            let mean = ts.iter().sum::<f64>() / n as f64;
            let var = ts.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / n as f64;
            let cov = var.sqrt() / mean;
            (med, cov)
        }

        // Known field names from Crystal exports
        let all_fields = ["Level", "Section", "Field22", "Field23", "Field38",
                          "Field39", "Field61", "Field73", "FieldG", "Text20"];

        // Configurations: keep 1, 3, or 10 columns
        let configs: Vec<(usize, Vec<&str>)> = vec![
            (1, vec!["Level"]),
            (3, vec!["Level", "Section", "Field22"]),
            (10, all_fields.to_vec()),
        ];

        println!("\n=== Column scaling test ({:.1} MB, {} runs) ===", mb, n);
        println!("{:>6} {:>8} {:>8} {:>8} {:>6}", "cols", "MB/s", "ms/MB", "cyc/f", "CoV%");
        println!("{}", "-".repeat(42));

        for (keep_count, keep_fields) in &configs {
            // Build drop list: all fields NOT in keep_fields
            let drop: Vec<String> = all_fields.iter()
                .filter(|f| !keep_fields.contains(f))
                .map(|f| f.to_string())
                .collect();

            let mut plan = rypipe_core::ExecutionPlan::new();
            for d in &drop {
                plan.drop_fields.insert(d.clone());
            }

            // Warmup
            {
                let mut tb = rypipe_core::TableBuilder::with_capacity(100_000);
                let mut plan_w = plan.clone();
                scan_chunk(&bytes, b"Details", &mut tb).unwrap();
            }

            let (t_med, t_cov) = bench(n, || {
                let mut tb = rypipe_core::TableBuilder::with_plan(100_000, std::sync::Arc::new(plan.clone()));
                scan_chunk(&bytes, b"Details", &mut tb).unwrap();
            });

            let ms_mb = t_med / mb * 1000.0;
            // Get row/field count from a single run
            let mut tb = rypipe_core::TableBuilder::with_plan(100_000, std::sync::Arc::new(plan.clone()));
            scan_chunk(&bytes, b"Details", &mut tb).unwrap();
            let rows = tb.num_rows();
            let cols = tb.num_columns();
            let fields = rows * cols;
            let cyc_f = t_med / 1000.0 * 3.8e9 / fields as f64;
            let mb_s = mb / t_med;

            println!("{:>6} {:>8.0} {:>8.2} {:>8.0} {:>5.1}%", cols, mb_s, ms_mb, cyc_f, t_cov * 100.0);
        }
    }

    /// Verify that resolve_and_put fires on the production path.
    #[test]
    fn perf_resolve_and_put_counter() {
        let path = match std::env::var("BENCH_FILE") {
            Ok(p) => p,
            Err(_) => {
                let candidate = "bench_data/test_10mb.xml";
                if std::path::Path::new(candidate).exists() {
                    candidate.to_string()
                } else {
                    eprintln!("skipping: BENCH_FILE not set and {candidate} not found");
                    return;
                }
            }
        };
        let bytes = std::fs::read(&path).expect("failed to read BENCH_FILE");
        let mb = bytes.len() as f64 / 1024.0 / 1024.0;

        // Reset counter (profiling only)
        #[cfg(feature = "profiling")]
        rypipe_core::RESOLVE_AND_PUT_COUNT.store(0, std::sync::atomic::Ordering::Relaxed);

        // Run full parse (production path)
        let mut tb = rypipe_core::TableBuilder::with_capacity(100_000);
        scan_chunk(&bytes, b"Details", &mut tb).unwrap();
        let _ = tb.finish().unwrap();

        #[cfg(feature = "profiling")]
        {
            let count = rypipe_core::RESOLVE_AND_PUT_COUNT.load(std::sync::atomic::Ordering::Relaxed);
            let rows = tb.num_rows();
            let cols = tb.num_columns();
            println!("\n=== resolve_and_put counter ===");
            println!("file: {:.1} MB, {} rows, {} cols", mb, rows, cols);
            println!("resolve_and_put calls: {count}");
            println!("assertion: count > 0");

            assert!(count > 0, "resolve_and_put was never called! Production may not be using the optimized path.");
            println!("PASS: resolve_and_put fires on the scanner path");
        }
        #[cfg(not(feature = "profiling"))]
        println!("resolve_and_put counter: profiling feature not enabled, skipping check");
    }

    struct TravFile;
    impl ColumnarSink for TravFile {
        #[inline] fn begin_row(&mut self) {}
        #[inline] fn put_field(&mut self, _n: &str, _v: Value<'_>) {}
        #[inline] fn end_row(&mut self) {}
        #[inline] fn wants(&self, _name: &str) -> bool { true }
        #[inline] fn needs_value(&self) -> bool { false }
        #[inline] fn needs_resolve(&self) -> bool { false }
        fn finish(&mut self) -> RResult<RecordBatch> {
            Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))
        }
    }

    struct LocateFile {
        plan: rypipe_core::ExecutionPlan,
    }
    impl ColumnarSink for LocateFile {
        #[inline] fn begin_row(&mut self) {}
        #[inline] fn put_field(&mut self, _n: &str, _v: Value<'_>) {}
        #[inline] fn end_row(&mut self) {}
        #[inline] fn wants(&self, _name: &str) -> bool { true }
        #[inline] fn needs_value(&self) -> bool { false }
        #[inline] fn resolve<'a>(&'a self, name: &'a str) -> Option<&'a str> {
            self.plan.resolve_field(name)
        }
        fn finish(&mut self) -> RResult<RecordBatch> {
            Ok(RecordBatch::new_empty(Arc::new(Schema::empty())))
        }
    }

    /// Regression test: asserts that the predicate skip fires correctly.
    /// Without this, the next refactor breaks predicate-first silently.
    ///
    /// ```
    /// cargo test --features profile predicate_skip_assertions -- --nocapture
    /// ```
    #[test]
    #[cfg(feature = "profile")]
    fn predicate_skip_assertions() {
        use rypipe_core::{ExecutionPlan, FilterPredicate, TableBuilder};

        // Generate 11-field rows (Level + Field1..Field11)
        let n_rows = 5_000usize;
        let mut xml = String::from("<?xml version=\"1.0\"?>\n<CrystalReport>");
        for i in 0..n_rows {
            use std::fmt::Write;
            write!(
                xml,
                "<Details Level=\"{}\"><Field Name=\"Field1\"><Value>val{}</Value></Field>\n",
                i % 3,
                i % 100,
            ).unwrap();
            xml.push_str("<Field Name=\"Field2\"><Value>v</Value></Field>\n");
            xml.push_str("<Field Name=\"Field3\"><Value>v</Value></Field>\n");
            xml.push_str("<Field Name=\"Field4\"><Value>v</Value></Field>\n");
            xml.push_str("<Field Name=\"Field5\"><Value>v</Value></Field>\n");
            write!(xml, "<Field Name=\"Field6\"><Value>val{}</Value></Field>\n", i % 100).unwrap();
            xml.push_str("<Field Name=\"Field7\"><Value>v</Value></Field>\n");
            xml.push_str("<Field Name=\"Field8\"><Value>v</Value></Field>\n");
            xml.push_str("<Field Name=\"Field9\"><Value>v</Value></Field>\n");
            xml.push_str("<Field Name=\"Field10\"><Value>v</Value></Field>\n");
            write!(xml, "<Field Name=\"Field11\"><Value>val{}</Value></Field>\n", i % 100).unwrap();
            xml.push_str("</Details>");
        }
        xml.push_str("</CrystalReport>");
        let bytes = xml.as_bytes();

        // Helper: run a filter and return (rejected, skipped)
        let run_with_filter = |filter: FilterPredicate| -> (u64, u64) {
            reset_profile_counters();
            let mut plan = ExecutionPlan::new();
            plan.filter = Some(filter);
            let mut tb = TableBuilder::with_plan(n_rows, std::sync::Arc::new(plan));
            scan_chunk(bytes, b"Details", &mut tb).unwrap();
            let _ = tb.finish();
            let rej = REJECTED_ROWS.load(Ordering::Relaxed);
            let skp = SKIPPED_FIELDS.load(Ordering::Relaxed);
            (rej, skp)
        };

        // first/0%: predicate on Field1, value "999" (doesn't exist)
        // All rows rejected, skip ratio = 10 fields per rejection
        let (rej, skp) = run_with_filter(FilterPredicate::Equal {
            field: "Field1".to_string(),
            value: "999".to_string(),
        });
        assert!(rej > 0, "first/0%: no rows rejected — filter not firing");
        assert_eq!(rej, n_rows as u64, "first/0%: should reject all {} rows, got {}", n_rows, rej);
        assert_eq!(skp / rej, 10, "first/0%: skip ratio should be 10, got {}", skp / rej);

        // middle/0%: predicate on Field6, value "999"
        let (rej, skp) = run_with_filter(FilterPredicate::Equal {
            field: "Field6".to_string(),
            value: "999".to_string(),
        });
        assert!(rej > 0, "middle/0%: no rows rejected");
        assert_eq!(rej, n_rows as u64, "middle/0%: should reject all {} rows, got {}", n_rows, rej);
        assert_eq!(skp / rej, 5, "middle/0%: skip ratio should be 5 (skip after Field6), got {}", skp / rej);

        // last/0%: predicate on Field11, value "999"
        let (rej, skp) = run_with_filter(FilterPredicate::Equal {
            field: "Field11".to_string(),
            value: "999".to_string(),
        });
        assert!(rej > 0, "last/0%: no rows rejected");
        assert_eq!(rej, n_rows as u64, "last/0%: should reject all {} rows, got {}", n_rows, rej);
        assert_eq!(skp / rej, 0, "last/0%: skip ratio should be 0 (late predicate, no skip), got {}", skp / rej);
    }

    /// Predicate-first diagnostic: generates synthetic CR XML, runs three
    /// filter configurations, and reports the profiling counter ratio.
    ///
    /// ```
    /// cargo test --features profile perf_predicate_first -- --nocapture
    /// ```
    #[test]
    fn perf_predicate_first() {
        use rypipe_core::{ExecutionPlan, FilterPredicate, TableBuilder};
        use std::time::Instant;

        // --- Generate synthetic data: 11 fields per row (matches real export) ---
        let n_rows = 20_000usize;
        let mut xml = Vec::with_capacity(n_rows * 1100);
        xml.extend_from_slice(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><CrystalReport>");
        for i in 0..n_rows {
            xml.extend_from_slice(
                format!(
                    "<Details Level=\"{}\">\
<Field Name=\"Field1\" FieldName=\"{{a.F1}}\"><Value>{}</Value></Field>\
<Field Name=\"Field2\" FieldName=\"{{a.F2}}\"><Value>{}</Value></Field>\
<Field Name=\"Field3\" FieldName=\"{{a.F3}}\"><Value>text3</Value></Field>\
<Field Name=\"Field4\" FieldName=\"{{a.F4}}\"><Value>text4</Value></Field>\
<Field Name=\"Field5\" FieldName=\"{{a.F5}}\"><Value>{}</Value></Field>\
<Field Name=\"Field6\" FieldName=\"{{a.F6}}\"><Value>text6</Value></Field>\
<Field Name=\"Field7\" FieldName=\"{{a.F7}}\"><Value>text7</Value></Field>\
<Field Name=\"Field8\" FieldName=\"{{a.F8}}\"><Value>text8</Value></Field>\
<Field Name=\"Field9\" FieldName=\"{{a.F9}}\"><Value>text9</Value></Field>\
<Field Name=\"Field10\" FieldName=\"{{a.F10}}\"><Value>text10</Value></Field>\
<Field Name=\"Field11\" FieldName=\"{{a.F11}}\"><Value>{}</Value></Field>\
</Details>",
                    i % 3,       // Level
                    i % 100,     // Field1: varies
                    i % 100,     // Field2: varies (same as Field1)
                    i % 200,     // Field5: varies
                    i % 100,     // Field11: varies
                )
                .as_bytes(),
            );
        }
        xml.extend_from_slice(b"</CrystalReport>");
        let mb = xml.len() as f64 / 1024.0 / 1024.0;

        // --- Helper: median-of-3 ---
        fn bench3(mut f: impl FnMut()) -> f64 {
            let mut ts: Vec<f64> = (0..3)
                .map(|_| {
                    let t0 = Instant::now();
                    f();
                    t0.elapsed().as_secs_f64()
                })
                .collect();
            ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
            ts[1]
        }

        // --- Unfiltered baseline ---
        let t_unfiltered = bench3(|| {
            let mut tb = TableBuilder::with_plan(n_rows, std::sync::Arc::new(ExecutionPlan::new()));
            scan_chunk(&xml, b"Details", &mut tb).unwrap();
            let _ = tb.finish();
        });
        let unfiltered_rows = {
            let mut tb = TableBuilder::with_plan(n_rows, std::sync::Arc::new(ExecutionPlan::new()));
            scan_chunk(&xml, b"Details", &mut tb).unwrap();
            tb.finish().unwrap().num_rows()
        };
        println!("\n=== Position sweep ({:.1} MB, {} rows, 11 fields) ===", mb, unfiltered_rows);
        println!("unfiltered        {:8.4}s  ratio 1.00", t_unfiltered);

        // Helper to run a filter case and report.
        #[cfg(feature = "profile")]
        let run_filter_case = |label: &str, filter: FilterPredicate, expect_rows: usize| {
            reset_profile_counters();
            let mut plan = ExecutionPlan::new();
            plan.filter = Some(filter);
            let t = bench3(|| {
                let mut tb = TableBuilder::with_plan(n_rows, std::sync::Arc::new(plan.clone()));
                scan_chunk(&xml, b"Details", &mut tb).unwrap();
                let _ = tb.finish();
            });
            let rows = {
                let mut tb = TableBuilder::with_plan(n_rows, std::sync::Arc::new(plan.clone()));
                scan_chunk(&xml, b"Details", &mut tb).unwrap();
                tb.finish().unwrap().num_rows()
            };
            let rejected = REJECTED_ROWS.load(Ordering::Relaxed);
            let skipped = SKIPPED_FIELDS.load(Ordering::Relaxed);
            let scanned = ROWS_SCANNED.load(Ordering::Relaxed);
            let checks = REJECTED_CHECKS.load(Ordering::Relaxed);
            let evals = rypipe_core::PREDICATE_EVALUATIONS.load(Ordering::Relaxed);
            let fails = rypipe_core::PREDICATE_FAILS.load(Ordering::Relaxed);
            let undecided = rypipe_core::PREDICATE_UNDECIDED.load(Ordering::Relaxed);
            let is_pred_t = rypipe_core::IS_PRED_TRUE.load(Ordering::Relaxed);
            let is_pred_f = rypipe_core::IS_PRED_FALSE.load(Ordering::Relaxed);
            let rpc = rypipe_core::RESOLVE_AND_PUT_COUNT.load(Ordering::Relaxed);
            let ratio = t_unfiltered / t;
            println!("  {:40} {:6.2}x  rows={:<6} expect={}  rej={:<6} skp={:<6} scan={:<6} chk={:<8} eval={:<5} fail={:<5} und={:<5} T={:<6} F={:<6} rpc={}", label, ratio, rows, expect_rows, rejected, skipped, scanned, checks, evals, fails, undecided, is_pred_t, is_pred_f, rpc);
        };
        #[cfg(not(feature = "profile"))]
        let run_filter_case = |label: &str, filter: FilterPredicate, expect_rows: usize| {
            let mut plan = ExecutionPlan::new();
            plan.filter = Some(filter);
            let t = bench3(|| {
                let mut tb = TableBuilder::with_plan(n_rows, std::sync::Arc::new(plan.clone()));
                scan_chunk(&xml, b"Details", &mut tb).unwrap();
                let _ = tb.finish();
            });
            let rows = {
                let mut tb = TableBuilder::with_plan(n_rows, std::sync::Arc::new(plan.clone()));
                scan_chunk(&xml, b"Details", &mut tb).unwrap();
                tb.finish().unwrap().num_rows()
            };
            let ratio = t_unfiltered / t;
            println!("  {:40} {:6.2}x  rows={:<6} expect={}", label, ratio, rows, expect_rows);
        };

        // Position sweep: first / middle / last field
        // 0% selectivity: value "999" doesn't exist → reject all
        // 100% selectivity: NotEqual with nonexistent → keep all
        // ~50% selectivity: Field == "0" → keeps ~1/3 (i%100==0 or i%200==0)
        let positions = [
            ("Field1",  "first ", n_rows / 100),  // i%100==0
            ("Field6",  "middle", n_rows / 3),     // Level field (i%3==0)
            ("Field11", "last  ", n_rows / 100),  // i%100==0
        ];
        for (field, pos_label, expect_0) in &positions {
            println!("\n--- {} (predicate on {}) ---", pos_label, field);
            run_filter_case(
                &format!("{}/0%%  (== nonexistent)", pos_label),
                FilterPredicate::Equal { field: field.to_string(), value: "999".to_string() },
                0,
            );
            run_filter_case(
                &format!("{}/100%% (!= nonexistent)", pos_label),
                FilterPredicate::NotEqual { field: field.to_string(), value: "999".to_string() },
                *expect_0,
            );
            // ~50%: use a value that actually exists
            let half_val = if *field == "Field6" { "0" } else { "0" };
            run_filter_case(
                &format!("{}/~50%%  (== \"{}\")", pos_label, half_val),
                FilterPredicate::Equal { field: field.to_string(), value: half_val.to_string() },
                *expect_0,
            );
        }

        println!("\n=== Acceptance ===");
        println!("1. 0% rows=0, 100% rows=unfiltered_rows (correctness)");
        println!("2. last/100% should be >=0.95x (C1 adaptive: no buffering overhead)");
        println!("3. first/0% should be >1x (C1 adaptive: skip fields)");
    }

    /// Allocation baseline: counts every malloc/free during a parse, reports
    /// per-field and per-row allocation pressure.
    ///
    /// ```
    /// BENCH_FILE=test_533mb.xml cargo test --features alloc-stats alloc_baseline -- --nocapture
    /// ```
    #[test]
    fn alloc_baseline() {
        #[cfg(not(feature = "alloc-stats"))]
        {
            eprintln!("skipping: requires --features alloc-stats");
            return;
        }

        #[cfg(feature = "alloc-stats")]
        {
            use rypipe_core::alloc_stats;
            use rypipe_core::{ExecutionPlan, FilterPredicate, TableBuilder, FieldType};
            use rypipe_core::Splitter;
            use std::time::Instant;

            let path = match std::env::var("BENCH_FILE") {
                Ok(p) => p,
                Err(_) => {
                    eprintln!("skipping: BENCH_FILE not set");
                    return;
                }
            };
            let bytes = std::fs::read(&path).expect("failed to read BENCH_FILE");
            let mb = bytes.len() as f64 / 1024.0 / 1024.0;

            let est_row = crate::xml::CrystalXmlSplitter::with_row_tag("Details")
                .estimate_bytes_per_row(&bytes[..bytes.len().min(65536)]);
            let cap = (bytes.len() / est_row.max(512)).max(64);

            // --- Single-thread unfiltered ---
            alloc_stats::reset();
            let before = alloc_stats::snapshot();
            let t0 = Instant::now();
            let mut tb = TableBuilder::with_plan(cap, std::sync::Arc::new(ExecutionPlan::new()));
            scan_chunk(&bytes, b"Details", &mut tb).unwrap();
            let batch = tb.finish().unwrap();
            let elapsed = t0.elapsed().as_secs_f64();
            let after = alloc_stats::snapshot();
            let delta = after.delta(&before);
            let rows = batch.num_rows();
            let ncols = batch.num_columns();

            alloc_stats::print_stats(&format!("UNFILTERED single ({:.1} MB, {} rows, {} cols, {:.2}s, {:.0} MB/s)",
                mb, rows, ncols, elapsed, mb / elapsed), &delta);
            println!("  allocs/row:  {:>10}", delta.allocs / rows.max(1) as u64);
            println!("  allocs/field: {:>9}", delta.allocs / (rows as u64 * ncols.max(1) as u64).max(1));
            println!("  bytes/row:  {:>10}", delta.bytes / rows.max(1) as u64);
            drop(batch);

            // --- Single-thread with filter (0% selectivity) ---
            {
                let mut plan = ExecutionPlan::new();
                // Filter on a late column to measure buffered path overhead.
                plan.filter = Some(FilterPredicate::Equal {
                    field: "Field61".to_string(),
                    value: "nonexistent".to_string(),
                });
                alloc_stats::reset();
                let before = alloc_stats::snapshot();
                let t0 = Instant::now();
                let mut tb = TableBuilder::with_plan(cap, std::sync::Arc::new(plan));
                scan_chunk(&bytes, b"Details", &mut tb).unwrap();
                let batch = tb.finish().unwrap();
                let elapsed = t0.elapsed().as_secs_f64();
                let after = alloc_stats::snapshot();
                let delta = after.delta(&before);
                let rows_out = batch.num_rows();
                alloc_stats::print_stats(&format!("FILTERED 0%% ({:.1} MB, {} rows out, {:.2}s, {:.0} MB/s)",
                    mb, rows_out, elapsed, mb / elapsed), &delta);
                println!("  allocs/row_in: {:>9}", delta.allocs / (rows as u64).max(1));
                drop(batch);
            }

            // --- Single-thread with typed columns ---
            {
                let mut plan = ExecutionPlan::new();
                for name in ["Field2", "Field5", "Field8", "Field11"] {
                    plan.field_types.insert(name.to_string(), FieldType::Int64);
                }
                alloc_stats::reset();
                let before = alloc_stats::snapshot();
                let t0 = Instant::now();
                let mut tb = TableBuilder::with_plan(cap, std::sync::Arc::new(plan));
                scan_chunk(&bytes, b"Details", &mut tb).unwrap();
                let batch = tb.finish().unwrap();
                let elapsed = t0.elapsed().as_secs_f64();
                let after = alloc_stats::snapshot();
                let delta = after.delta(&before);
                alloc_stats::print_stats(&format!("TYPED int64 ({:.1} MB, {} rows, {:.2}s, {:.0} MB/s)",
                    mb, batch.num_rows(), elapsed, mb / elapsed), &delta);
                println!("  allocs/row:  {:>10}", delta.allocs / batch.num_rows().max(1) as u64);
                // Verify field_types were applied
                for (i, name) in batch.schema().fields().iter().enumerate() {
                    println!("    col[{}] {}: {:?}", i, name.name(), name.data_type());
                }
                drop(batch);
            }

            // --- Determinism check: run twice, counts must match ---
            {
                let mut results = Vec::new();
                for run in 0..2 {
                    alloc_stats::reset();
                    let before = alloc_stats::snapshot();
                    let mut tb = TableBuilder::with_plan(cap, std::sync::Arc::new(ExecutionPlan::new()));
                    scan_chunk(&bytes, b"Details", &mut tb).unwrap();
                    let _ = tb.finish().unwrap();
                    let after = alloc_stats::snapshot();
                    let delta = after.delta(&before);
                    results.push(delta.allocs);
                    println!("  determinism run {}: allocs={}", run, delta.allocs);
                }
                if results[0] != results[1] {
                    eprintln!("  WARNING: allocs differ across runs ({} vs {}) — non-deterministic!",
                        results[0], results[1]);
                    eprintln!("  This means a HashMap iteration order or size-dependent branch affects a code path.");
                } else {
                    println!("  PASS: allocation counts identical across runs ({})", results[0]);
                }
            }
        }
    }
}


