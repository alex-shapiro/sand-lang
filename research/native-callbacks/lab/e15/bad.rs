#![forbid(unsafe_code)]
include!("common.rs");
fn native(vm: &mut Vm) -> String {
    let v = vm.arg(0);
    vm.call_nested();
    vm.get(v).to_string()
}
fn main() { println!("{}", native(&mut Vm::new())); }
