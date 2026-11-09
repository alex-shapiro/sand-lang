//! Prototype P1: nested native->Sand callbacks (`Vm::call_nested` on the Rust stack).
use sandlang::engine::{Config, HostModules, Limits};
use sandlang::native::NativeModule;
use sandlang::native::ty::{Param, Val};
use sandlang::session::Session;
use sandlang::vm::value::Value;
use sandlang::vm::{Ctl, Vm};
use std::time::Instant;

const SORTBENCH: &str = include_str!("../sortbench.sand");

fn module() -> NativeModule {
    let mut m = NativeModule::new("host");
    m.vm_function("repeat_", |vm: &mut Vm, f: Val<fn(i64) -> i64>, n: i64| -> Result<i64, Ctl> {
        let mut acc = 0;
        for i in 0..n {
            if let Value::Int(x) = vm.call_nested(f.raw(), &[Value::Int(i)])? { acc += x; }
        }
        Ok(acc)
    });
    // The same bottom-up merge sort as P2 and as the Sand version, in direct style:
    // one nested comparator call per comparison. The element handles live in Rust `Vec`s.
    m.vm_function("sort_with", |vm: &mut Vm, xs: Val<Vec<Param<0>>>, less: Val<fn(Param<0>, Param<0>) -> bool>| -> Result<(), Ctl> {
        let mut src = vm.heap.list(xs.raw()).to_vec();
        let mut dst = src.clone();
        let n = src.len();
        let mut width = 1;
        while width < n {
            let mut lo = 0;
            while lo < n {
                let (mid, hi) = ((lo + width).min(n), (lo + 2 * width).min(n));
                let (mut i, mut j, mut k) = (lo, mid, lo);
                while i < mid && j < hi {
                    if vm.call_nested(less.raw(), &[src[j], src[i]])? == Value::Bool(true) { dst[k] = src[j]; j += 1; } else { dst[k] = src[i]; i += 1; }
                    k += 1;
                }
                while i < mid { dst[k] = src[i]; i += 1; k += 1; }
                while j < hi { dst[k] = src[j]; j += 1; k += 1; }
                lo = hi;
            }
            std::mem::swap(&mut src, &mut dst);
            width *= 2;
        }
        *vm.heap.list_mut(xs.raw()) = src;
        Ok(())
    });
    m.vm_function("call", |vm: &mut Vm, f: Val<fn() -> Param<0>>| -> Result<Param<0>, Ctl> {
        Ok(Param::from_raw(vm.call_nested(f.raw(), &[])?))
    });
    m.vm_function("bounce", |vm: &mut Vm, f: Val<fn(i64) -> i64>, d: i64| -> Result<i64, Ctl> {
        match vm.call_nested(f.raw(), &[Value::Int(d)])? { Value::Int(x) => Ok(x), _ => Ok(0) }
    });
    // Calls f, then allocates a list of f() elements. It checks memory before
    // it allocates, as SPEC §16.3 requires; the size depends on the callback.
    m.vm_function("effect_then_alloc", |vm: &mut Vm, f: Val<fn() -> i64>| -> Result<Val<Vec<i64>>, Ctl> {
        let Value::Int(n) = vm.call_nested(f.raw(), &[])? else { return Err(Ctl::internal("not an int")) };
        vm.check_memory(n as usize * 16)?;
        Ok(Val::from_raw(vm.heap.new_list(vec![Value::Int(0); n as usize])))
    });
    m.source("pub native fn effect_then_alloc(f: fn() -> int) -> [int]\npub native fn repeat_(f: fn(int) -> int, n: int) -> int\npub native fn sort_with<T>(xs: [T], less: fn(T, T) -> bool)\npub native fn call<T>(f: fn() -> T) -> T\npub native fn bounce(f: fn(int) -> int, d: int) -> int");
    m
}

fn median(mut v: Vec<f64>) -> f64 { v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[v.len() / 2] }

async fn time(s: &mut Session, src: &str, n: u64) -> (f64, String) {
    let t = Instant::now();
    let r = s.eval(src).await;
    (t.elapsed().as_secs_f64() / n as f64 * 1e9, format!("{:?}", r.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string())))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("== sandlang prototype P1 (nested calls), fiber parking: {}", cfg!(feature = "nested"));
    let limits = Limits { max_instructions: None, max_cpu_time: None, max_wall_time: None, ..Limits::default() };
    let config = |l: Limits| Config::new().host_modules(HostModules::new([module()]).unwrap()).limits(l);
    let only_gc = std::env::args().nth(1).as_deref() == Some("gc");
    if !only_gc {
        let mut s = Session::new(config(limits.clone()));
        let n = 5_000_000u64;
        s.eval("import host\nlet inc_value: fn(int) -> int = x => x + 1").await.unwrap();
        let (mut v, mut c) = (vec![], vec![]);
        let mut out = String::new();
        for _ in 0..5 {
            v.push(time(&mut s, &format!("var a = 0\nfor i in 0..{n} {{ a += inc_value(i) }}\na"), n).await.0);
            let (t, r) = time(&mut s, &format!("host.repeat_(inc_value, {n})"), n).await;
            c.push(t); out = r;
        }
        println!("E1 script->script call through fn value: {:.1} ns/iter; native->script callback loop: {:.1} ns/iter (result {out})", median(v), median(c));
        s.eval(SORTBENCH).await.unwrap();
        let r = s.eval("\"native merge sort + nested Sand comparator (P1) \\(bench(xs => host.sort_with(xs, (a, b) => a < b))); merge sort written in Sand \\(bench(xs => merge_sort(xs, (a, b) => a < b)))\"").await;
        println!("E6 sort 200000 ints, median of 5: {:?}", r.map_err(|e| e.to_string()));
        let r = time(&mut s, "var ticks = 0\nlet bg = spawn { for _ in 0..3 { ticks += 1\n task.sleep(1ms) } }\nlet v = host.call(() => { task.sleep(10ms)\n ticks * 100 })\nbg.wait()\nv", 1).await.1;
        println!("E3 callback that sleeps 10 ms: {r}");
        for d in [100, 199, 200, 1000] {
            let r = time(&mut s, &format!("fn f(d: int) -> int {{ if d >= {d} {{ return d }}\n host.bounce(f, d + 1) }}\nf(0)"), 1).await.1;
            println!("E2 native<->script recursion to depth {d}: {r}");
        }
        let mut s4 = Session::new(config(Limits { max_instructions: Some(10_000_000), ..limits.clone() }));
        let r = time(&mut s4, "import host\nhost.call(() => { var i = 0\n while true { i += 1 }\n i })", 1).await.1;
        println!("E4 instruction limit inside callback: {r}");
    }
    // E11: GC inside a nested callback must not free the caller's live values.
    let mut s6 = Session::new(Config::new().host_modules(HostModules::new([module()]).unwrap()).gc_stress(true).limits(limits.clone()));
    let r = time(&mut s6, "import host\nfn work() -> bool {\n let keep = (0..50).map(i => [i, i + 1])\n let r = host.call(() => { var junk: [[int]] = []\n for i in 0..200 { junk.push([i]) }\n junk.len() })\n r == 200 and keep[49][1] == 50 and keep.len() == 50\n}\nwork()", 1).await.1;
    println!("E11 caller's locals after a GC inside a nested callback: {r}");
    // E12: the native's copied element handles are not roots: a comparator that empties the list
    let r = time(&mut s6, "import host\nlet t = (0..64).map(i => [64 - i])\nhost.sort_with(t, (a, b) => { t.clear()\n var junk: [[int]] = []\n for i in 0..50 { junk.push([i]) }\n a[0] < b[0] })\nt.map(x => x[0])", 1).await.1;
    println!("E12 comparator that empties the list under gc_stress: {}", &r[..r.len().min(200)]);
    // E13: the engine re-runs a vm_function after a failed memory check (retry_stress fails the first check)
    let mut s7 = Session::new(Config::new().host_modules(HostModules::new([module()]).unwrap()).retry_stress(true).limits(limits.clone()));
    let r = s7.eval("import host\nvar calls = 0\nlet v = host.effect_then_alloc(() => { calls += 1\n 3 })\n\"callback ran \\(calls) times, native returned \\(v)\"").await;
    println!("E13 callback with a side effect, then a native step that checks memory and is retried: {:?}", r.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string()));
}
