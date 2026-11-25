//! Arm 3 collectors leave containers of other engines alone.

use vm_gc::arm3::value::Value;
use vm_gc::arm3::vm::Vm;
use vm_gc::arm3::{codec, host};
use vm_gc::shared::Limits;
use vm_gc::shared::programs::{Variant, program};
use vm_gc::shared::suite::HostConfig;

#[test]
fn a_foreign_container_in_a_garbage_cycle_keeps_live_data_intact() {
    let modules = || vec![codec::core_module(), host::host_module(&HostConfig::default())];
    let mut a = Vm::new(program(Variant::Structural), Limits::default(), modules()).unwrap();
    let mut b = Vm::new(program(Variant::Structural), Limits::default(), modules()).unwrap();
    // Each engine's first container takes slot 0 of its table.
    let foreign = a.new_list(Vec::new()).unwrap();
    let live = b.new_list(vec![Value::Int(1)]).unwrap();
    let p = b.new_list(vec![foreign.clone()]).unwrap();
    let q = b.new_list(vec![p.clone()]).unwrap();
    b.list_push(&p, q.clone()).unwrap();
    drop((p, q));
    b.collect();
    let Value::List(items) = &live else { panic!() };
    assert!(matches!(items.lock().unwrap().0[..], [Value::Int(1)]), "the collector must not count a foreign reference against slot 0");
    // A container grows only in the engine that tracks its bytes.
    assert!(b.list_push(&foreign, Value::Int(2)).is_err());
    a.collect();
    drop(foreign);
}
