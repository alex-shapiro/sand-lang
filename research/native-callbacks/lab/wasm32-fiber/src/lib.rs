//! Calls a guest function through wasmtime's async API, which runs the guest on a fiber.
#![no_std]
extern crate alloc;

pub async fn run(bytes: &[u8]) -> wasmtime::Result<i32> {
    let engine = wasmtime::Engine::default();
    let module = unsafe { wasmtime::Module::deserialize(&engine, bytes)? };
    let mut store = wasmtime::Store::new(&engine, ());
    let instance = wasmtime::Instance::new_async(&mut store, &module, &[]).await?;
    let f = instance.get_typed_func::<(), i32>(&mut store, "f")?;
    f.call_async(&mut store, ()).await
}
