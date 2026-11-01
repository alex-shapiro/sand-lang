//! Fuzz target that compiles and runs arbitrary UTF-8 input under resource
//! limits, asserting that the engine never returns an internal error and that
//! the run prints and returns the same under GC stress, unless a limit ends
//! either run.
#![no_main]
use libfuzzer_sys::fuzz_target;
use sandlang::{Config, Engine, Error, Limits, MemorySink, RulePolicy};
use std::time::Duration;

fuzz_target!(|data: &[u8]| {
    let Ok(src) = std::str::from_utf8(data) else { return };
    let run = |gc_stress: bool| {
        let out = MemorySink::new();
        let config = Config::new()
            .policy(RulePolicy::standard())
            .stdout(out.clone())
            .stderr(MemorySink::new())
            .seed(7)
            .gc_stress(gc_stress)
            .limits(Limits {
                max_instructions: Some(100_000),
                max_memory: Some(16 << 20),
                max_wall_time: Some(Duration::from_secs(1)),
                max_call_depth: 256,
                ..Limits::default()
            });
        let mut engine = Engine::new(config).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let r = rt.block_on(engine.run_source("fuzz.sand", src));
        if let Err(e @ Error::Internal { .. }) = &r {
            panic!("{e}");
        }
        (matches!(r, Err(Error::Limit(_))), format!("{r:?}"), out.contents())
    };
    let plain = run(false);
    if plain.0 {
        return;
    }
    let stressed = run(true);
    if !stressed.0 {
        assert_eq!(plain, stressed);
    }
});
