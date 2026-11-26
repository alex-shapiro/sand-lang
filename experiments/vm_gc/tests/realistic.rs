//! The ports of `bench/programs` give the outputs of the original programs.

use vm_gc::shared::realistic::run_port;
use vm_gc::shared::suite::ArmEngine;

fn binary_trees_output(max_depth: i64) -> String {
    let check = |depth: i64| (1i64 << (depth + 1)) - 1;
    let mut out = format!("stretch tree of depth {}\t check: {}\n", max_depth + 1, check(max_depth + 1));
    for depth in (4..=max_depth).step_by(2) {
        let iterations = 1i64 << (max_depth - depth + 4);
        out += &format!("{iterations}\t trees of depth {depth}\t check: {}\n", iterations * check(depth));
    }
    out + &format!("long lived tree of depth {max_depth}\t check: {}\n", check(max_depth))
}

fn check<E: ArmEngine>() {
    let output = |name, size| run_port::<E>(name, size).unwrap_or_else(|e| panic!("{}: {name}: {e}", E::NAME)).0;
    assert_eq!(output("binary_trees", 8), binary_trees_output(8));
    assert_eq!(output("fannkuch", 7), "228\nPfannkuchen(7) = 16\n");
    assert!(output("nbody", 1000).starts_with("-0.169075164\n"));
    assert_eq!(output("richards", 2), "iterations: 2\npackets queued: 46492\nholds: 18594\n");
}

#[test]
fn ports_match_the_original_programs() {
    check::<vm_gc::arm1::Engine>();
    check::<vm_gc::arm2::Engine>();
    check::<vm_gc::arm3::Engine>();
    let reference = run_port::<vm_gc::arm1::Engine>("nbody", 1000).unwrap().0;
    assert_eq!(run_port::<vm_gc::arm2::Engine>("nbody", 1000).unwrap().0, reference);
    assert_eq!(run_port::<vm_gc::arm3::Engine>("nbody", 1000).unwrap().0, reference);
}
