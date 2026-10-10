//! Formatting is idempotent and its output always re-parses.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else { return };
    if let Ok(once) = sandlang::fmt::format_source(src) {
        let twice = sandlang::fmt::format_source(&once).expect("formatted output must parse");
        assert_eq!(once, twice);
        let before = sandlang::fmt::meaning(src).expect("formatted sources parse");
        assert_eq!(before, sandlang::fmt::meaning(&once).expect("formatted output must parse"), "formatting changed the meaning");
    }
});
