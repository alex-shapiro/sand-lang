#![forbid(unsafe_code)]
include!("common.rs");
fn native(vm: &mut Vm) -> String {
    let h = { let v = vm.arg(0); vm.root(v) };
    vm.call_nested();
    let v = vm.load(&h);
    vm.get(v).to_string()
}
fn main() { println!("E15 rooted handle across the nested call: {}", native(&mut Vm::new())); }
