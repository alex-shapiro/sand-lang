use wasmtime::*;

const WAT: &str = r#"
(module
  (import "host" "sleep" (func $sleep (result i32)))
  (import "host" "call_handler" (func $call_handler (result i32)))
  (func (export "handler") (result i32) (i32.add (call $sleep) (i32.const 1)))
  (func (export "main") (result i32) (call $call_handler)))"#;

const COMPONENT: &str = r#"
(component
  (import "cb" (func $cb))
  (core module $m
    (import "h" "cb" (func $cb))
    (func (export "run") (call $cb))
    (func (export "noop")))
  (core func $cb_lower (canon lower (func $cb)))
  (core instance $i (instantiate $m (with "h" (instance (export "cb" (func $cb_lower))))))
  (func (export "run") (canon lift (core func $i "run")))
  (func (export "noop") (canon lift (core func $i "noop"))))"#;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    println!("== wasmtime 50.0.1 async and component model");
    let engine = Engine::default();
    let module = Module::new(&engine, WAT)?;
    let mut linker = Linker::<()>::new(&engine);
    // An async host function: suspends the guest until a timer fires.
    linker.func_wrap_async("host", "sleep", |_caller: Caller<'_, ()>, (): ()| Box::new(async move {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        Ok(41i32)
    }))?;
    // An async host function that calls back into the guest; the callback suspends again.
    linker.func_wrap_async("host", "call_handler", |mut caller: Caller<'_, ()>, (): ()| Box::new(async move {
        let h = caller.get_export("handler").unwrap().into_func().unwrap().typed::<(), i32>(&caller)?;
        h.call_async(&mut caller, ()).await
    }))?;
    let mut store = Store::new(&engine, ());
    let inst = linker.instantiate_async(&mut store, &module).await?;
    let main = inst.get_typed_func::<(), i32>(&mut store, "main")?;
    let t = std::time::Instant::now();
    let r = main.call_async(&mut store, ()).await;
    println!("E3 guest -> async host -> guest callback -> async host (sleeps 5 ms): {:?} in {:?}", r.map_err(|e| e.to_string()), t.elapsed());
    let r = main.call(&mut store, ());
    println!("E3b blocking call on an async store: {:?}", r.map_err(|e| e.to_string()));

    // Component Model: the host re-enters a component instance while that instance waits on an import.
    use wasmtime::component::{Component, Instance, Linker as CLinker};
    let engine = Engine::default();
    let component = Component::new(&engine, COMPONENT)?;
    let mut linker = CLinker::<Option<Instance>>::new(&engine);
    linker.root().func_wrap("cb", |mut store: StoreContextMut<'_, Option<Instance>>, (): ()| {
        let inst = store.data().unwrap();
        let noop = inst.get_typed_func::<(), ()>(&mut store, "noop")?;
        noop.call(&mut store, ())?;
        noop.post_return(&mut store)?;
        Ok(())
    })?;
    let mut store = Store::new(&engine, None);
    let inst = linker.instantiate(&mut store, &component)?;
    *store.data_mut() = Some(inst);
    let run = inst.get_typed_func::<(), ()>(&mut store, "run")?;
    let r = run.call(&mut store, ());
    println!("E10 component re-entered during an import call: {:?}", r.map_err(|e| format!("{e:?}").lines().next().unwrap_or("").to_string()));
    Ok(())
}
