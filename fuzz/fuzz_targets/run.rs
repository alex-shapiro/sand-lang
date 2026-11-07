//! Fuzz target that compiles and runs arbitrary UTF-8 input under resource
//! limits, asserting that the engine never returns an internal error.
#![no_main]
use libfuzzer_sys::fuzz_target;
use sandlang::engine::{Config, Engine, Error, Limits, MemorySink, Run};
use sandlang::policy::rules::RulePolicy;
use std::time::Duration;

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else { return };
    let config = Config::new()
        .policy(RulePolicy::standard())
        .limits(Limits {
            max_instructions: Some(100_000),
            max_memory: Some(16 << 20),
            max_wall_time: Some(Duration::from_secs(1)),
            max_call_depth: 256,
            ..Limits::default()
        });
    let mut engine = Engine::new(config);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    if let Err(e @ Error::Internal { .. }) = rt.block_on(engine.run_source("fuzz.sand", src, Run::new().stdout(MemorySink::new()).stderr(MemorySink::new()))) {
        panic!("{e}");
    }
});
