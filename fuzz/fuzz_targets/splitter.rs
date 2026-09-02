#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Feed arbitrary bytes to the splitter; must not panic or UB.
    let splitter = crxml_core::xml::CrystalXmlSplitter::with_row_tag(b"Details");
    let decoder = crxml_core::xml::CrystalXmlDecoder::with_row_tag(b"Details");

    // Test next_record_start with arbitrary positions
    for pos in (0..data.len()).step_by(100) {
        let _ = splitter.next_record_start(data, pos);
    }

    // Test find_split_points
    let points = splitter.find_split_points(data, 4);
    assert!(points.first() == Some(&0));
    assert!(points.last() == Some(&data.len()));
    assert!(points.windows(2).all(|w| w[0] < w[1]));

    // Every chunk, if non-empty, must not panic when fed to the parser.
    use rypipe_core::decoder::RecordParser;
    for window in points.windows(2) {
        let chunk = &data[window[0]..window[1]];
        if !chunk.is_empty() {
            let mut sink = rypipe_core::engine::LocateOnly::new(rypipe_core::ExecutionPlan::new());
            let _ = decoder.parse_chunk(chunk, &mut sink);
        }
    }
});
