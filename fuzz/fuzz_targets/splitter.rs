#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Feed arbitrary bytes to the splitter — must not panic or UB.
    let tag = b"Details";
    let (skip_regions, has_special) = crxml_core::splitter::find_special_regions(data);
    let _ = crxml_core::splitter::next_row_start(data, 0, tag, &skip_regions);
    let _ = crxml_core::splitter::compute_splits(data, tag, 4);

    // If the data is valid enough for compute_splits to produce chunks,
    // verify round-trip: every byte belongs to exactly one chunk.
    let chunks = crxml_core::splitter::compute_splits(data, tag, 4);
    if chunks.len() > 1 {
        // chunks are [0..a, a..b, b..c, c..len] after reduce
        assert_eq!(chunks[0].start, 0);
        assert_eq!(chunks[chunks.len() - 1].end, data.len());
        for w in chunks.windows(2) {
            assert_eq!(w[0].end, w[1].start, "chunks must be contiguous");
        }
    }

    // Every chunk, if non-empty, must start with a row tag or be a valid
    // continuation (can start with raw text if mid-row due to unmatched
    // parent end tag). The only invariant: a non-empty chunk does not panic
    // when fed to the columnar engine.
    for chunk in &chunks {
        if !chunk.is_empty() {
            let mut engine = crxml_core::columnar::ColumnarEngine::new();
            // Ignore parse errors — fuzz input is arbitrary.
            let _ = engine.parse_bytes(&data[chunk.clone()], tag);
        }
    }
});
