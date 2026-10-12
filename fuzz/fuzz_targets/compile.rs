//! Arbitrary source text must produce diagnostics, never a host panic.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(src) = std::str::from_utf8(data) {
        let engine = sandlang::Engine::new(sandlang::Config::new());
        let _ = engine.check_source("fuzz.sand", src);
    }
});
