use std::ops::Range;

/// Phase A: locate comment and CDATA regions that contain false-positive `<Row` markers.
///
/// Returns sorted, non-overlapping byte ranges to skip during split-point
/// validation. In practice CR exports rarely contain either construct, so
/// the returned `has_any` flag lets callers skip range-check logic entirely.
pub fn find_special_regions(bytes: &[u8]) -> (Vec<Range<usize>>, bool) {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut pos = 0;

    while pos < bytes.len() {
        let remaining = &bytes[pos..];

        // Comment: <!-- ... -->
        if let Some(rel) = memchr::memmem::find(remaining, b"<!--") {
            let start = pos + rel;
            let after_open = start + 4;
            if let Some(rel_close) = memchr::memmem::find(&bytes[after_open..], b"-->") {
                let end = after_open + rel_close + 3;
                ranges.push(start..end);
                pos = end;
                continue;
            }
            // Unclosed comment — treat rest of file as comment
            ranges.push(start..bytes.len());
            return (ranges, true);
        }

        // CDATA: <![CDATA[ ... ]]>
        if let Some(rel) = memchr::memmem::find(remaining, b"<![CDATA[") {
            let start = pos + rel;
            let after_open = start + 9;
            if let Some(rel_close) = memchr::memmem::find(&bytes[after_open..], b"]]>") {
                let end = after_open + rel_close + 3;
                ranges.push(start..end);
                pos = end;
                continue;
            }
            // Unclosed CDATA — treat rest of file as CDATA
            ranges.push(start..bytes.len());
            return (ranges, true);
        }

        break;
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
pub fn next_row_start(
    bytes: &[u8],
    from: usize,
    tag: &[u8],
    skip: &[Range<usize>],
) -> Option<usize> {
    if from >= bytes.len() {
        return None;
    }
    let mut p = from;
    while let Some(rel) = memchr::memchr(b'<', &bytes[p..]) {
        let at = p + rel;
        if bytes[at + 1..].starts_with(tag) {
            let after = at + 1 + tag.len();
            let boundary = matches!(
                bytes.get(after),
                Some(b' ' | b'\t' | b'\r' | b'\n' | b'>' | b'/')
            );
            let in_skip = skip.iter().any(|r| r.contains(&at));
            if boundary && !in_skip {
                return Some(at);
            }
        }
        p = at + 1;
    }
    None
}

/// Compute N chunk byte ranges from `bytes`, each containing a whole number
/// of complete rows.
///
/// Falls back to a single `0..bytes.len()` chunk when:
/// - `num_chunks <= 1`
/// - the file is too small to produce that many non-empty chunks
/// - no valid split point could be found for a nominal boundary
pub fn compute_splits(bytes: &[u8], row_tag: &[u8], num_chunks: usize) -> Vec<Range<usize>> {
    if num_chunks <= 1 || bytes.is_empty() {
        return vec![0..bytes.len()];
    }

    let (skip, _) = find_special_regions(bytes);
    let mut split_points: Vec<usize> = Vec::with_capacity(num_chunks + 1);
    split_points.push(0);

    for i in 1..num_chunks {
        let nominal = bytes.len() * i / num_chunks;
        match next_row_start(bytes, nominal, row_tag, &skip) {
            Some(at) => {
                // Deduplicate: if this split point is the same as the last one
                // (happens when multiple nominals land in the same gap), push
                // forward to the *next* valid row start instead.
                if at == *split_points.last().unwrap() {
                    if let Some(next) = next_row_start(bytes, at + 1, row_tag, &skip) {
                        split_points.push(next);
                    } else {
                        // No more row starts — use single chunk
                        return vec![0..bytes.len()];
                    }
                } else {
                    split_points.push(at);
                }
            }
            None => {
                // Fewer rows than chunks — fall back
                return vec![0..bytes.len()];
            }
        }
    }

    split_points.push(bytes.len());

    // Build ranges from split points
    let mut ranges: Vec<Range<usize>> = Vec::with_capacity(num_chunks);
    for i in 0..split_points.len() - 1 {
        let start = split_points[i];
        let end = split_points[i + 1];
        if start < end {
            ranges.push(start..end);
        }
        // Skip empty ranges (adjacent identical split points)
    }

    if ranges.is_empty() {
        return vec![0..bytes.len()];
    }
    ranges
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
        // Find the inner <Row> inside the comment (after first `<!--` marker)
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
        // <Row A="1"/><Row B="2"/>
        // 0         1         2
        // 012345678901234567890123
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/>";
        // First <Row> at 0
        assert_eq!(next_row_start(xml, 0, b"Row", &[]), Some(0));
        // Search after first <Row> — finds second at 12
        assert_eq!(next_row_start(xml, 1, b"Row", &[]), Some(12));
        // Search after end — None
        assert_eq!(next_row_start(xml, 100, b"Row", &[]), None);
    }

    #[test]
    fn test_next_row_start_prefix_collision() {
        // <RowItem should NOT match when looking for <Row
        // <RowItem X="1"/> = 16 bytes, then <Row A="1"/> at offset 16
        let xml = b"<RowItem X=\"1\"/><Row A=\"1\"/>";
        assert_eq!(next_row_start(xml, 0, b"Row", &[]), Some(16));
    }

    #[test]
    fn test_next_row_start_skips_comment() {
        let xml = b"<!-- <Row> --><Row A=\"1\"/>";
        let (skip, _) = find_special_regions(xml);
        let start = next_row_start(xml, 0, b"Row", &skip).unwrap();
        assert!(start > xml.len() / 2); // Should be the one after the comment
    }

    #[test]
    fn test_compute_splits_single_chunk() {
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/><Row C=\"3\"/>";
        let ranges = compute_splits(xml, b"Row", 1);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 0..xml.len());
    }

    #[test]
    fn test_compute_splits_two_chunks() {
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/><Row C=\"3\"/><Row D=\"4\"/>";
        let ranges = compute_splits(xml, b"Row", 2);
        assert_eq!(ranges.len(), 2);
        // Both ranges should be non-empty
        assert!(ranges[0].len() > 0);
        assert!(ranges[1].len() > 0);
        // They should cover the whole file
        assert_eq!(ranges[0].start, 0);
        assert_eq!(ranges[1].end, xml.len());
        assert_eq!(ranges[0].end, ranges[1].start);
    }

    #[test]
    fn test_compute_splits_fallback_small_file() {
        // File with only 2 rows but asking for 8 chunks → fallback to single chunk
        let xml = b"<Row A=\"1\"/><Row B=\"2\"/>";
        let ranges = compute_splits(xml, b"Row", 8);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 0..xml.len());
    }
}
