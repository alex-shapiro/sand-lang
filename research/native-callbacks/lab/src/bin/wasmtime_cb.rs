use wasmtime::Result;
use callback_lab::{N, median_ns};
use wasmtime::*;

const WAT: &str = r#"
(module
  (import "host" "repeat" (func $repeat (param i64) (result i64)))
  (import "host" "bounce" (func $bounce (param i64) (result i64)))
  (import "host" "spin_cb" (func $spin_cb))
  (func $inc (export "inc") (param i64) (result i64) local.get 0 i64.const 1 i64.add)
  ;; script->script loop: calls $inc n times
  (func (export "loop_call") (param $n i64) (result i64) (local $i i64) (local $acc i64)
    (block $done (loop $l
      (br_if $done (i64.ge_u (local.get $i) (local.get $n)))
      (local.set $acc (i64.add (local.get $acc) (call $inc (local.get $i))))
      (local.set $i (i64.add (local.get $i) (i64.const 1))) (br $l)))
    local.get $acc)
  (func (export "loop_native") (param $n i64) (result i64) (call $repeat (local.get $n)))
  ;; native<->script recursion
  (func $f (export "f") (param $d i64) (result i64) (call $bounce (i64.add (local.get $d) (i64.const 1))))
  (func (export "spin") (loop $l (br $l)))
  (func (export "spin_via_host") (call $spin_cb))
)"#;

struct Host { depth: i64 }

fn main() -> Result<()> {
    println!("== wasmtime {}", "50.0.1");
    let mut config = Config::new();
    config.consume_fuel(true);
    let engine = Engine::new(&config)?;
    let module = Module::new(&engine, WAT)?;
    let mut linker = Linker::<Host>::new(&engine);
    linker.func_wrap("host", "repeat", |mut caller: Caller<'_, Host>, n: i64| -> Result<i64> {
        let inc = caller.get_export("inc").unwrap().into_func().unwrap().typed::<i64, i64>(&caller)?;
        let mut acc = 0i64;
        for i in 0..n { acc += inc.call(&mut caller, i)?; }
        Ok(acc)
    })?;
    linker.func_wrap("host", "bounce", |mut caller: Caller<'_, Host>, d: i64| -> Result<i64> {
        caller.data_mut().depth = d;
        let f = caller.get_export("f").unwrap().into_func().unwrap().typed::<i64, i64>(&caller)?;
        f.call(&mut caller, d)
    })?;
    linker.func_wrap("host", "spin_cb", |mut caller: Caller<'_, Host>| -> Result<()> {
        let f = caller.get_export("spin").unwrap().into_func().unwrap().typed::<(), ()>(&caller)?;
        f.call(&mut caller, ())
    })?;
    let mut store = Store::new(&engine, Host { depth: 0 });
    store.set_fuel(u64::MAX)?;
    let inst = linker.instantiate(&mut store, &module)?;
    let lc = inst.get_typed_func::<i64, i64>(&mut store, "loop_call")?;
    let ln = inst.get_typed_func::<i64, i64>(&mut store, "loop_native")?;
    let s2s = median_ns(5, N, || { lc.call(&mut store, N as i64).unwrap(); });
    let n2s = median_ns(5, N, || { ln.call(&mut store, N as i64).unwrap(); });
    println!("E1 (JIT) script->script call loop: {s2s:.1} ns/iter; native->script callback loop: {n2s:.1} ns/iter");
    // E2: depth
    let f = inst.get_typed_func::<i64, i64>(&mut store, "f")?;
    let r = f.call(&mut store, 0);
    println!("E2 native<->script max depth: {} | {}", store.data().depth, format!("{:?}", r.err().map(|e| e.root_cause().to_string())));
    // E4: fuel keeps applying in code that a host function calls
    store.set_fuel(10_000_000)?;
    let s = inst.get_typed_func::<(), ()>(&mut store, "spin_via_host")?;
    let r = s.call(&mut store, ());
    println!("E4 fuel inside callback: {:?}", r.err().map(|e| e.root_cause().to_string()));
    Ok(())
}
