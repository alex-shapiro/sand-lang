use callback_lab::{N, median_ns};
use rune::runtime::{Function, VmResult, budget};
use rune::{Context, Module, Source, Sources, Vm};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

static DEPTH: AtomicI64 = AtomicI64::new(0);

fn repeat_(f: Function, n: i64) -> VmResult<i64> {
    let mut acc = 0;
    for i in 0..n {
        acc += rune::vm_try!(f.call::<i64>((i,)));
    }
    VmResult::Ok(acc)
}

fn bounce(f: Function, d: i64) -> VmResult<i64> {
    DEPTH.store(d, Ordering::Relaxed);
    f.call::<i64>((d,))
}

fn vm(src: &str) -> anyhow::Result<Vm> {
    let mut m = Module::new();
    m.function("repeat_", repeat_).build()?;
    m.function("bounce", bounce).build()?;
    let mut context = Context::with_default_modules()?;
    context.install(m)?;
    let runtime = Arc::new(context.runtime()?);
    let mut sources = Sources::new();
    sources.insert(Source::memory(src)?)?;
    let unit = rune::prepare(&mut sources).with_context(&context).build()?;
    Ok(Vm::new(runtime, Arc::new(unit)))
}

fn main() -> anyhow::Result<()> {
    println!("== rune 0.14.2");
    let n = (N / 10) as i64;
    let mut v = vm("pub fn inc(x) { x + 1 }
        pub fn loop0(n) { let a = 0; for i in 0..n { a += i + 1; } a }
        pub fn loopc(n) { let a = 0; for i in 0..n { a += inc(i); } a }
        pub fn loopn(n) { repeat_(inc, n) }
        pub fn f(d) { bounce(f, d + 1) }
        pub fn spin(x) { loop {} }
        pub fn spin_cb() { repeat_(spin, 1) }
        pub async fn handler(x) { x }
        pub fn async_cb() { repeat_(handler, 1) }")?;
    if std::env::args().nth(1).as_deref() == Some("budget") {
    // E4: control: the budget stops a loop that runs directly in the VM
    let r = budget::with(1_000_000, || v.call(["spin"], (0i64,))).call();
    println!("E4 budget, loop called directly: {:?}", r.map(|x| format!("{x:?}")).map_err(|e| e.to_string()));
    // E4: the same loop reached through a native function's Function::call
    let r = budget::with(1_000_000, || v.call(["spin_cb"], ())).call();
    println!("E4 budget inside callback: {:?}", r.map(|x| format!("{x:?}")).map_err(|e| e.to_string()));
        return Ok(());
    }
    let mut t = |name: &str| median_ns(5, n as u64, || { v.call([name], (n,)).unwrap(); });
    let (b, s, c) = (t("loop0"), t("loopc"), t("loopn"));
    println!("E1 loop without call: {b:.1} ns/iter; script->script call loop: {s:.1} ns/iter; native->script callback loop: {c:.1} ns/iter");
    // E3: an async callback returns a future that a synchronous native cannot await
    let r = v.call(["async_cb"], ());
    println!("E3 async callback from sync native: {:?}", r.map(|x| format!("{x:?}")).map_err(|e| e.to_string()));
    println!("E2 starting unbounded native<->script recursion...");
    let r = v.call(["f"], (0i64,));
    println!("E2 native<->script max depth: {} | {:?}", DEPTH.load(Ordering::Relaxed), r.err().map(|e| e.to_string()));
    Ok(())
}
