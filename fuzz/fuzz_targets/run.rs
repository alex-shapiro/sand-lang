//! Programs that compile must run to completion or a contained error under tight limits.
#![no_main]
use libfuzzer_sys::fuzz_target;
use sandlang::{Config, Engine, Limits, MemorySink, Policy};
use std::time::Duration;

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else { return };
    let config = Config::new()
        .policy(Policy::deny_all())
        .stdout(MemorySink::new())
        .stderr(MemorySink::new())
        .limits(Limits {
            max_instructions: Some(100_000),
            max_memory: Some(16 << 20),
            max_wall_time: Some(Duration::from_secs(1)),
            max_call_depth: 256,
            ..Limits::default()
        });
    let mut engine = Engine::new(config);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let _ = rt.block_on(engine.run_source("fuzz.sand", src));
});
