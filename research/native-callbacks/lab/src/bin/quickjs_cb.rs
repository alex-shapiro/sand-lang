use callback_lab::{N, median_ns};
use rquickjs::{Context, Function, Runtime, Ctx, CatchResultExt};
use std::sync::atomic::{AtomicI64, AtomicBool, Ordering};
use std::sync::Arc;

static DEPTH: AtomicI64 = AtomicI64::new(0);

fn main() {
    println!("== QuickJS-NG 0.16.2 via rquickjs 0.14.0");
    let rt = Runtime::new().unwrap();
    let ctx = Context::full(&rt).unwrap();
    ctx.with(|ctx: Ctx| {
        let g = ctx.globals();
        g.set("repeat_", Function::new(ctx.clone(), |f: Function, n: i64| -> rquickjs::Result<i64> {
            let mut acc = 0i64;
            for i in 0..n { acc += f.call::<_, i64>((i,))?; }
            Ok(acc)
        }).unwrap()).unwrap();
        g.set("bounce", Function::new(ctx.clone(), |f: Function, d: i64| -> rquickjs::Result<i64> {
            DEPTH.store(d, Ordering::Relaxed);
            f.call::<_, i64>((d,))
        }).unwrap()).unwrap();
        g.set("call_type", Function::new(ctx.clone(), |f: Function| -> rquickjs::Result<String> {
            let v: rquickjs::Value = f.call(())?;
            Ok(v.type_name().to_string())
        }).unwrap()).unwrap();
        ctx.eval::<(), _>("globalThis.inc = x => x + 1;
            globalThis.loop0 = n => { let a = 0; for (let i = 0; i < n; i++) a += i + 1; return a; };
            globalThis.loopc = n => { let a = 0; for (let i = 0; i < n; i++) a += inc(i); return a; };").unwrap();
        let loop0: Function = g.get("loop0").unwrap();
        let loopc: Function = g.get("loopc").unwrap();
        let rep: Function = g.get("repeat_").unwrap();
        let inc: Function = g.get("inc").unwrap();
        let n = N as i64;
        let b = median_ns(5, N, || { loop0.call::<_, i64>((n,)).unwrap(); });
        let s = median_ns(5, N, || { loopc.call::<_, i64>((n,)).unwrap(); });
        let c = median_ns(5, N, || { rep.call::<_, i64>((inc.clone(), n)).unwrap(); });
        println!("E1 loop without call: {b:.1} ns/iter; script->script call loop: {s:.1} ns/iter; native->script callback loop: {c:.1} ns/iter");
        let r = ctx.eval::<rquickjs::Value, _>("function f(d) { return bounce(f, d + 1); } try { f(0) } catch (e) { String(e) }").catch(&ctx);
        println!("E2 native<->script max depth: {} | {:?}", DEPTH.load(Ordering::Relaxed), r.map(|v| v.as_string().map(|s| s.to_string().unwrap())));
        let r = ctx.eval::<rquickjs::Value, _>("let sd = 0; function g(d) { sd = d; return 1 + g(d + 1); } try { g(0) } catch (e) { 'script-only depth ' + sd + ' ' + String(e) }").catch(&ctx);
        println!("E2 {:?}", r.map(|v| v.as_string().map(|s| s.to_string().unwrap())));
    });
    // E4: interrupt handler fires inside a callback
    let on = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicI64::new(0));
    let (on2, c2) = (on.clone(), count.clone());
    rt.set_interrupt_handler(Some(Box::new(move || on2.load(Ordering::Relaxed) && c2.fetch_add(1, Ordering::Relaxed) > 100_000)));
    ctx.with(|ctx: Ctx| {
        on.store(true, Ordering::Relaxed);
        let r = ctx.eval::<rquickjs::Value, _>("try { repeat_(x => { for (;;) {} }, 1) } catch (e) { 'script caught: ' + e }").catch(&ctx);
        println!("E4 interrupt inside callback: {:?}", r.map(|v| v.type_name()).map_err(|e| e.to_string()));
        on.store(false, Ordering::Relaxed);
    });
    rt.set_interrupt_handler(None);
    // E5: memory limit inside a callback
    rt.set_memory_limit(8 << 20);
    ctx.with(|ctx: Ctx| {
        let r = ctx.eval::<rquickjs::Value, _>("try { repeat_(x => { const a = []; for (;;) a.push('x'.repeat(100) + a.length); }, 1) } catch (e) { 'caught: ' + e }").catch(&ctx);
        println!("E5 memory limit inside callback: {:?}", r.map(|v| v.as_string().map(|s| s.to_string().unwrap())).map_err(|e| e.to_string()));
    });
    rt.set_memory_limit(0);
    // E3: a promise-returning callback cannot be awaited by a synchronous native; jobs run only when the host drains them
    ctx.with(|ctx: Ctx| {
        let r = ctx.eval::<rquickjs::Value, _>("globalThis.log = []; const t = call_type(async () => { await null; log.push('callback resumed'); return 1 }); log.push('native saw ' + t); log.join(',')").catch(&ctx);
        println!("E3 async callback from native: {:?}", r.map(|v| v.as_string().map(|s| s.to_string().unwrap())).map_err(|e| e.to_string()));
    });
    while rt.execute_pending_job().unwrap_or(false) {}
    ctx.with(|ctx: Ctx| {
        let r: String = ctx.eval("log.join(',')").unwrap();
        println!("E3 after host drains jobs: {r}");
    });
}
