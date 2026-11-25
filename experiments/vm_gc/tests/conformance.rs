//! Every arm gives the same output, fuel use, and custom-method calls for
//! the same B2 and B3 work, so the arms run the same algorithm.

use vm_gc::shared::Limits;
use vm_gc::shared::programs::Variant;
use vm_gc::shared::suite::{Arg, ArmEngine, HostValue, engine, int};

/// What one arm reports for a workload: each call's result, fuel use, and
/// custom-method calls.
fn run<E: ArmEngine>(variant: Variant) -> Vec<(HostValue, u64, u64)> {
    let mut e: E = engine(variant, Limits::default());
    let calls: [(&str, Vec<Arg>); 6] = [
        ("setup_doc", vec![int(50)]),
        ("encode_doc", vec![]),
        ("setup_text", vec![int(50)]),
        ("roundtrip_text", vec![]),
        ("setup_address", vec![]),
        ("bench_small_decode", vec![int(20)]),
    ];
    calls
        .iter()
        .map(|(name, args)| {
            let before = e.stats();
            let result = e.call(name, args).unwrap_or_else(|err| panic!("{}: {name}: {err}", E::NAME));
            let after = e.stats();
            (result, before.fuel_left - after.fuel_left, after.custom_calls - before.custom_calls)
        })
        .collect()
}

#[test]
fn arms_agree_on_output_fuel_and_callbacks() {
    for variant in [Variant::Structural, Variant::Custom] {
        let arm1 = run::<vm_gc::arm1::Engine>(variant);
        assert_eq!(arm1, run::<vm_gc::arm2::Engine>(variant), "arm2 differs from arm1 for {variant:?}");
        assert_eq!(arm1, run::<vm_gc::arm3::Engine>(variant), "arm3 differs from arm1 for {variant:?}");
    }
}
