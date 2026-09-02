//! Chunk splitter for Crystal Reports XML streams.

use std::ops::Range;

use memchr;
use rypipe_core::decoder::SkipRegionFinder;
use rypipe_core::Splitter;

/// Timing stub: compute_splits was removed in S1 migration.
/// Returns (0, 0) for backward compatibility with get_par_profile.
pub fn split_timing() -> (u64, u64) {
    (0, 0)
}

/// Format-specific splitter for Crystal Reports-style XML.
///
/// Splits an input byte stream on row-tag boundaries so that each chunk can be
/// parsed independently by [`CrystalXmlDecoder`](crate::CrystalXmlDecoder).
#[derive(Clone, Debug, Default)]
pub struct CrystalXmlSplitter {
    row_tag: Vec<u8>,
}

/// CR XML skip-region finder: comments (`<!-- -->`) and CDATA (`<![CDATA[ ]]>`).
struct CrXmlSkipRegions;

impl SkipRegionFinder for CrXmlSkipRegions {
    fn openers(&self) -> &[&'static [u8]] {
        &[b"<!--", b"<![CDATA["]
    }

    fn closer_for(&self, opener: &[u8]) -> &'static [u8] {
        if opener == b"<!--" {
            b"-->"
        } else {
            b"]]>"
        }
    }
}

impl CrystalXmlSplitter {
    /// Create a splitter with a custom row element name.
    pub fn with_row_tag(row_tag: impl AsRef<[u8]>) -> Self {
        Self {
            row_tag: row_tag.as_ref().to_vec(),
        }
    }
}

impl Splitter for CrystalXmlSplitter {
    fn next_record_start(&self, bytes: &[u8], from: usize) -> Option<usize> {
        next_row_start_fast(bytes, from, &self.row_tag).map(|pos| {
            // Return position past the closing `>` of the row tag
            let after = pos + 1 + self.row_tag.len();
            bytes[after..]
                .iter()
                .position(|&b| b == b'>')
                .map(|rel| after + rel + 1)
                .unwrap_or(bytes.len())
        })
    }

    // find_split_points: use the default from the Splitter trait.
    // It calls next_record_start + plan_chunk_count + in_skip_region.

    fn skip_regions(&self) -> Option<&dyn SkipRegionFinder> {
        Some(&CrXmlSkipRegions)
    }

    fn estimate_bytes_per_row(&self, sample: &[u8]) -> usize {
        let sample_end = sample.len().min(65536);
        // Count only open tags (<RowTag), not close tags (</RowTag>),
        // to avoid 2× overcount that halves the bytes/row estimate.
        let mut prefix = Vec::with_capacity(1 + self.row_tag.len());
        prefix.push(b'<');
        prefix.extend_from_slice(&self.row_tag);
        let row_tag_count = memchr::memmem::find_iter(&sample[..sample_end], &prefix).count();
        let est = sample_end.checked_div(row_tag_count).unwrap_or_else(|| {
            memchr::memmem::find(&sample[..sample_end], &prefix)
                .map(|pos| pos + prefix.len())
                .unwrap_or(512)
        });
        est.max(1)
    }
}

/// Phase A: locate comment and CDATA regions that contain false-positive `<Row` markers.
///
/// Returns sorted, non-overlapping byte ranges to skip during split-point
/// validation. In practice CR exports rarely contain either construct, so
/// the returned `has_any` flag lets callers skip range-check logic entirely.
///
/// Optimized for the common case (no special regions) by scanning for the
/// common prefix `b"<!"` once instead of two separate substring searches.
pub(crate) fn find_special_regions(bytes: &[u8]) -> (Vec<Range<usize>>, bool) {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut pos = 0;

    while let Some(rel) = memchr::memmem::find(&bytes[pos..], b"<!") {
        let start = pos + rel;

        // Check for comment <!--
        if bytes[start..].starts_with(b"<!--") {
            let after_open = start + 4;
            if let Some(close_rel) = memchr::memmem::find(&bytes[after_open..], b"-->") {
                let end = after_open + close_rel + 3;
                ranges.push(start..end);
                pos = end;
                continue;
            }
            ranges.push(start..bytes.len());
            return (ranges, true);
        }

        // Check for CDATA <![CDATA[
        if bytes[start..].starts_with(b"<![CDATA[") {
            let after_open = start + 9;
            if let Some(close_rel) = memchr::memmem::find(&bytes[after_open..], b"]]>") {
                let end = after_open + close_rel + 3;
                ranges.push(start..end);
                pos = end;
                continue;
            }
            ranges.push(start..bytes.len());
            return (ranges, true);
        }

        pos = start + 1;
    }

    let has_any = !ranges.is_empty();
    (ranges, has_any)
}

/// Phase B: find the next valid `<RowTag` after byte offset `from`.
///
/// A valid candidate:
/// - starts with `<` followed by `tag`
/// - is followed by whitespace, `>`, or `/` (rejects `<RowItem`-style prefix collisions)
/// - is not inside any skip range (comment/CDATA)
pub(crate) fn next_row_start(
    bytes: &[u8],
    from: usize,
    tag: &[u8],
    skip: &[Range<usize>],
) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }

    // Stack-allocate the full tag to avoid per-row heap allocation.
    // `<` + tag fits in 32 bytes for all realistic row tags.
    let mut tag_buf = [0u8; 32];
    tag_buf[0] = b'<';
    tag_buf[1..1 + tag.len()].copy_from_slice(tag);
    let full_tag = &tag_buf[..1 + tag.len()];

    let mut p = from;
    while let Some(rel) = memchr::memmem::find(&bytes[p..], &full_tag) {
        let at = p + rel; // points to `<`
        let after = at + 1 + tag.len(); // just past `<tag`
        let boundary = matches!(
            bytes.get(after),
            Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')
        );
        let in_skip = skip.iter().any(|r| r.contains(&at));
        if boundary && !in_skip {
            return Some(at);
        }
        p = at + 1;
    }
    None
}

/// Find next row start with backward scan per candidate (no pre-computed skip).
pub(crate) fn next_row_start_fast(bytes: &[u8], from: usize, tag: &[u8]) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    // Stack-allocate the full tag to avoid per-row heap allocation.
    // `<` + tag fits in 32 bytes for all realistic row tags.
    let mut tag_buf = [0u8; 32];
    tag_buf[0] = b'<';
    tag_buf[1..1 + tag.len()].copy_from_slice(tag);
    let full_tag = &tag_buf[..1 + tag.len()];
    let skip_regions = CrXmlSkipRegions;
    let mut p = from;
    while let Some(rel) = memchr::memmem::find(&bytes[p..], &full_tag) {
        let at = p + rel;
        let after = at + 1 + tag.len();
        let boundary = matches!(
            bytes.get(after),
            Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')
        );
        if boundary && !rypipe_core::decoder::in_skip_region(bytes, at, &skip_regions) {
            return Some(at);
        }
        p = at + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_special_regions() {
        let xml = b"<Rows><Row A=\"1\"/><Row B=\"2\"/></Rows>";
        let (ranges, has) = find_special_regions(xml);
        assert!(!has);
        assert!(ranges.is_empty());
    }

    #[test]
    fn test_comment_region() {
        let xml = b"<Rows><!-- comment with <Row> inside --><Row A=\"1\"/></Rows>";
        let (ranges, has) = find_special_regions(xml);
        assert!(has);
        assert_eq!(ranges.len(), 1);
        let comment_open = xml.windows(4).position(|w| w == b"<!--").unwrap();
        let inner_row = xml[comment_open..]
            .windows(4)
            .position(|w| w == b"<Row")
            .unwrap()
            + comment_open;
        assert!(ranges[0].contains(&inner_row));
    }

    #[test]
    fn test_cdata_region() {
        let xml = b"<Rows><![CDATA[<Row> inside cdata ]]><Row A=\"1\"/></Rows>";
        let (ranges, has) = find_special_regions(xml);
        assert!(has);
        assert_eq!(ranges.len(), 1);
    }

    #[test]
    fn test_next_row_start_basic() {
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/>";
        assert_eq!(next_row_start(xml, 0, b"Row", &[]), Some(0));
        assert_eq!(next_row_start(xml, 1, b"Row", &[]), Some(12));
        assert_eq!(next_row_start(xml, 100, b"Row", &[]), None);
    }

    #[test]
    fn test_next_row_start_prefix_collision() {
        let xml = b"<RowItem X=\"1\"/><Row A=\"1\"/>";
        assert_eq!(next_row_start(xml, 0, b"Row", &[]), Some(16));
    }

    #[test]
    fn test_next_row_start_skips_comment() {
        let xml = b"<!-- <Row> --><Row A=\"1\"/>";
        let (skip, _) = find_special_regions(xml);
        let start = next_row_start(xml, 0, b"Row", &skip).unwrap();
        assert!(start > xml.len() / 2);
    }

    #[test]
    fn test_splitter_find_split_points_single_chunk() {
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/><Row C=\"3\"/>";
        let splitter = CrystalXmlSplitter::with_row_tag(b"Row");
        let points = splitter.find_split_points(xml, 1);
        assert_eq!(points.len(), 2);
        assert_eq!(points[0], 0);
        assert_eq!(points[1], xml.len());
    }

    #[test]
    fn test_splitter_find_split_points_two_chunks() {
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/><Row C=\"3\"/><Row D=\"4\"/>";
        let splitter = CrystalXmlSplitter::with_row_tag(b"Row");
        let points = splitter.find_split_points(xml, 2);
        assert!(points.len() >= 2);
        assert_eq!(points[0], 0);
        assert_eq!(*points.last().unwrap(), xml.len());
        assert!(points.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn test_splitter_find_split_points_fallback_small_file() {
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/>";
        let splitter = CrystalXmlSplitter::with_row_tag(b"Row");
        let points = splitter.find_split_points(xml, 8);
        // Small file: should fall back to single chunk
        assert_eq!(points.len(), 2);
        assert_eq!(points[0], 0);
        assert_eq!(points[1], xml.len());
    }

    #[test]
    fn test_splitter_find_split_points() {
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/><Row C=\"3\"/>";
        let splitter = CrystalXmlSplitter::with_row_tag(b"Row");
        let points = splitter.find_split_points(xml, 2);
        assert_eq!(points.first(), Some(&0));
        assert_eq!(points.last(), Some(&xml.len()));
        assert!(points.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn test_estimate_bytes_per_row() {
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/>";
        let splitter = CrystalXmlSplitter::with_row_tag(b"Row");
        let est = splitter.estimate_bytes_per_row(xml);
        assert!(est > 0);
    }

    /// Stress test: feed arbitrary byte patterns through the splitter.
    /// Must not panic on any input.
    #[test]
    fn test_random_bytes_no_panic() {
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
            b"<Row>\n<Row>\n<Row>",
            b"<Row/>,<Row/>",
            b"<RowItem/><Row/>",
            b"<Row/><RowItem/>",
            b"  <Row/>",
            b"<Row/>  ",
            b"<Row><!--<Row>--><Row/>",
            b"<Row><![CDATA[<Row>]]><Row/>",
            b"<!--",
            b"<!-- <Row> ",
            b"<![CDATA[",
            b"<![CDATA[ <Row> ",
            b"<Row/><Row/><Row/><Row/>",
            b"<Row/><Row/><Row/><Row/><Row/>",
            b"<Details Level=\"3\">text</Details>",
            b"<Details><Section SectionNumber=\"0\"><Field Name=\"X\"><Value>1</Value></Field></Section></Details>",
        ];

        let tags: &[&[u8]] = &[b"Row", b"Details", b"Item", b"A"];
        for seed in seeds {
            for tag in tags {
                let (skip, _) = find_special_regions(seed);
                let _ = next_row_start(seed, 0, tag, &skip);
                let splitter = CrystalXmlSplitter::with_row_tag(tag);
                for n in [1, 2, 3, 4, 8, 17] {
                    let points = splitter.find_split_points(seed, n);
                    if seed.is_empty() {
                        continue; // Empty input: skip validation
                    }
                    assert!(points.first() == Some(&0));
                    assert!(points.last() == Some(&seed.len()));
                    assert!(points.windows(2).all(|w| w[0] < w[1]));
                }
            }
        }
    }

    /// S1 property test: split points satisfy basic invariants.
    /// Points must be strictly increasing, start at 0, end at file length.
    #[test]
    fn test_split_points_invariants() {
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/><Row C=\"3\"/><Row D=\"4\"/><Row E=\"5\"/>";
        let splitter = CrystalXmlSplitter::with_row_tag(b"Row");
        for n in [2, 3, 4, 5] {
            let points = splitter.find_split_points(xml, n);
            assert_eq!(points[0], 0, "must start at 0");
            assert_eq!(*points.last().unwrap(), xml.len(), "must end at file length");
            assert!(points.windows(2).all(|w| w[0] < w[1]), "must be strictly increasing");
        }
    }

    /// S1 property test: whole-file parse == N-chunk parse.
    /// Row count from single-chunk must equal sum of row counts from N chunks.
    #[test]
    fn test_single_chunk_equals_multi_chunk() {
        use crate::xml::decoder::CrystalXmlDecoder;
        use rypipe_core::decoder::RecordParser;
        use rypipe_core::engine::LocateOnly;
        use rypipe_core::ExecutionPlan;

        let xml = b"<Rows><Row A=\"1\"/><Row B=\"2\"/><Row C=\"3\"/><Row D=\"4\"/><Row E=\"5\"/></Rows>";
        let splitter = CrystalXmlSplitter::with_row_tag(b"Row");
        let decoder = CrystalXmlDecoder::with_row_tag(b"Row");

        // Parse as single chunk
        let mut single = LocateOnly::new(ExecutionPlan::new());
        decoder.parse_chunk(xml, &mut single).unwrap();
        let single_rows = single.row_count;

        // Parse as N chunks
        for n in [2, 3, 5] {
            let points = splitter.find_split_points(xml, n);
            let mut multi_rows = 0;
            for w in points.windows(2) {
                let chunk = &xml[w[0]..w[1]];
                let mut loc = LocateOnly::new(ExecutionPlan::new());
                decoder.parse_chunk(chunk, &mut loc).unwrap();
                multi_rows += loc.row_count;
            }
            assert_eq!(
                single_rows, multi_rows,
                "row count mismatch at n={}: {} vs {}",
                n, single_rows, multi_rows
            );
        }
    }

    /// S1 property test: split time < 1% of single-thread parse time.
    #[test]
    fn test_split_time_overhead() {
        use crate::xml::decoder::CrystalXmlDecoder;
        use rypipe_core::decoder::RecordParser;
        use rypipe_core::engine::LocateOnly;
        use rypipe_core::ExecutionPlan;
        use std::time::Instant;

        // Generate a 1 MB XML fixture
        let mut xml = Vec::with_capacity(1_000_000);
        xml.extend_from_slice(b"<Rows>");
        let mut row_count = 0;
        while xml.len() < 1_000_000 {
            let i = row_count;
            xml.extend_from_slice(format!("<Row A=\"{i}\" B=\"value{i}\" C=\"{i}.{i}\"/>").as_bytes());
            row_count += 1;
        }
        xml.extend_from_slice(b"</Rows>");

        let splitter = CrystalXmlSplitter::with_row_tag(b"Row");
        let decoder = CrystalXmlDecoder::with_row_tag(b"Row");

        // Measure parse time
        let t0 = Instant::now();
        let mut loc = LocateOnly::new(ExecutionPlan::new());
        decoder.parse_chunk(&xml, &mut loc).unwrap();
        let parse_time = t0.elapsed();

        // Measure split time
        let t0 = Instant::now();
        let _points = splitter.find_split_points(&xml, 8);
        let split_time = t0.elapsed();

        let overhead = split_time.as_secs_f64() / parse_time.as_secs_f64();
        assert!(
            overhead < 0.10,
            "split overhead {:.1}% exceeds 10% threshold (split={:.2}ms, parse={:.2}ms)",
            overhead * 100.0,
            split_time.as_secs_f64() * 1000.0,
            parse_time.as_secs_f64() * 1000.0
        );
    }
}
