//! Prints how deep each arm nests on a 512 KB stack in the test profile.
//!
//! `cargo test --test depth -- --ignored --nocapture`

use vm_gc::shared::Limits;
use vm_gc::shared::programs::Variant;
use vm_gc::shared::suite::{ArmEngine, engine, int, on_small_stack};

/// Returns the largest `n` for which `function(n)` succeeds.
fn max_depth<E: ArmEngine>(variant: Variant, function: &'static str) -> i64 {
    on_small_stack(move || {
        let mut e: E = engine(variant, Limits::default());
        let (mut low, mut high) = (1i64, 100_000i64);
        while low < high {
            let mid = (low + high + 1) / 2;
            if e.call(function, &[int(mid)]).is_ok() { low = mid } else { high = mid - 1 }
        }
        low
    })
}

fn report<E: ArmEngine>() {
    println!(
        "{}: structural round trip {}, structural encode {}, callback round trip {}, callback encode {}",
        E::NAME,
        max_depth::<E>(Variant::Structural, "node_roundtrip"),
        max_depth::<E>(Variant::Structural, "node_encode"),
        max_depth::<E>(Variant::NodeCustom, "node_roundtrip"),
        max_depth::<E>(Variant::NodeCustom, "node_encode"),
    );
}

#[test]
#[ignore]
fn nesting_depth_on_a_small_stack() {
    report::<vm_gc::arm1::Engine>();
    report::<vm_gc::arm2::Engine>();
    report::<vm_gc::arm3::Engine>();
}
