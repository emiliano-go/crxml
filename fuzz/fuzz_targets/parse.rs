#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Feed arbitrary bytes to the full columnar parse path; must not
    // panic or UB. Malformed input returns Err, which we ignore.
    let tag = b"Details";
    let mut engine = crxml_core::columnar::ColumnarEngine::new();
    let _ = engine.parse_bytes(data, tag);
});
