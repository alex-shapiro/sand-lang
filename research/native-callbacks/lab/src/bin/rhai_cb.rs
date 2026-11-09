use callback_lab::{N, median_ns};
use rhai::{Dynamic, Engine, EvalAltResult, FnPtr, NativeCallContext};
use std::sync::atomic::{AtomicI64, Ordering};

static DEPTH: AtomicI64 = AtomicI64::new(0);

fn engine() -> Engine {
    let mut e = Engine::new();
    e.register_fn("repeat_", |ctx: NativeCallContext, f: FnPtr, n: i64| -> Result<i64, Box<EvalAltResult>> {
        let mut acc = 0;
        for i in 0..n { acc += f.call_within_context::<i64>(&ctx, (i,))?; }
        Ok(acc)
    });
    e.register_fn("bounce", |ctx: NativeCallContext, f: FnPtr, d: i64| -> Result<i64, Box<EvalAltResult>> {
        DEPTH.store(d, Ordering::Relaxed);
        f.call_within_context::<i64>(&ctx, (d,))
    });
    e
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    let mut e = engine();
    if arg == "deep" {
        // Raise the script call-depth limit far above the Rust stack.
        e.set_max_call_levels(usize::MAX);
        let r = e.eval::<i64>("fn f(d) { bounce(f, d + 1) } f(0)");
        println!("E2b native<->script recursion with no call limit: depth {} {:?}", DEPTH.load(Ordering::Relaxed), r.err().map(|x| x.to_string()));
        return;
    }
    println!("== rhai 1.27.0 (release build: default max_call_levels 64)");
    let n = N / 10;
    let ast0 = e.compile(format!("let a = 0; for i in 0..{n} {{ a += i + 1; }} a")).unwrap();
    let ast1 = e.compile(format!("fn inc(x) {{ x + 1 }} let a = 0; for i in 0..{n} {{ a += inc(i); }} a")).unwrap();
    let ast2 = e.compile(format!("fn inc(x) {{ x + 1 }} repeat_(inc, {n})")).unwrap();
    let b = median_ns(5, n, || { e.eval_ast::<i64>(&ast0).unwrap(); });
    let s = median_ns(5, n, || { e.eval_ast::<i64>(&ast1).unwrap(); });
    let c = median_ns(5, n, || { e.eval_ast::<i64>(&ast2).unwrap(); });
    println!("E1 loop without call: {b:.1} ns/iter; script->script call loop: {s:.1} ns/iter; native->script callback loop: {c:.1} ns/iter");
    let r = e.eval::<i64>("fn f(d) { bounce(f, d + 1) } f(0)");
    println!("E2 native<->script max depth: {} | {:?}", DEPTH.load(Ordering::Relaxed), r.err().map(|x| x.to_string()));
    // E4: operation-count progress callback fires inside a callback
    e.on_progress(|ops| if ops > 1_000_000 { Some(Dynamic::from("fuel exhausted")) } else { None });
    let r = e.eval::<i64>("fn spin(x) { loop {} } repeat_(spin, 1)");
    println!("E4 progress limit inside callback: {:?}", r.err().map(|x| x.to_string()));
}
