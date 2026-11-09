//! Prototype P2: stackless native->Sand callbacks (`Ctl::call` continuations on the fiber).
use sandlang::engine::{Config, HostModules, Limits};
use sandlang::native::NativeModule;
use sandlang::native::ty::{Param, Val};
use sandlang::session::Session;
use sandlang::vm::value::Value;
use sandlang::vm::{Ctl, Vm};
use std::time::Instant;

static CONTINUATION_RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const SORTBENCH: &str = include_str!("../sortbench.sand");

/// A bottom-up merge sort that asks for one comparison at a time.
struct MergeSort { n: usize, width: usize, lo: usize, mid: usize, hi: usize, i: usize, j: usize, k: usize }

impl MergeSort {
    fn new(n: usize) -> MergeSort { MergeSort { n, width: 1, lo: 0, mid: 0, hi: 0, i: 0, j: 0, k: 0 } }
    fn start_run(&mut self) {
        self.mid = (self.lo + self.width).min(self.n);
        self.hi = (self.lo + 2 * self.width).min(self.n);
        (self.i, self.j, self.k) = (self.lo, self.mid, self.lo);
    }
    /// Advances with the answer to "is src[j] < src[i]?"; returns the next pair to compare, or `None` when sorted.
    /// `state` is [list, src, dst, less].
    fn advance(&mut self, vm: &mut Vm, state: &mut [Value], answer: Option<bool>) -> Option<(Value, Value)> {
        if let Some(take_right) = answer {
            let (src, dst) = (state[1], state[2]);
            let v = if take_right { let v = vm.heap.list(src)[self.j]; self.j += 1; v } else { let v = vm.heap.list(src)[self.i]; self.i += 1; v };
            vm.heap.list_mut(dst)[self.k] = v;
            self.k += 1;
        }
        loop {
            if self.width >= self.n {
                let sorted = vm.heap.list(state[1]).to_vec();
                *vm.heap.list_mut(state[0]) = sorted;
                return None;
            }
            if self.lo >= self.n {
                state.swap(1, 2);
                self.width *= 2;
                self.lo = 0;
                self.start_run();
                continue;
            }
            if self.i < self.mid && self.j < self.hi {
                let src = vm.heap.list(state[1]);
                return Some((src[self.j], src[self.i]));
            }
            let (src, dst) = (state[1], state[2]);
            while self.i < self.mid { let v = vm.heap.list(src)[self.i]; vm.heap.list_mut(dst)[self.k] = v; self.i += 1; self.k += 1; }
            while self.j < self.hi { let v = vm.heap.list(src)[self.j]; vm.heap.list_mut(dst)[self.k] = v; self.j += 1; self.k += 1; }
            self.lo = self.hi;
            if self.lo < self.n { self.start_run(); }
        }
    }
}

fn module() -> NativeModule {
    let mut m = NativeModule::new("host");
    // Calls f(i) for i in 0..n and sums the results.
    m.vm_function("repeat_", |_vm: &mut Vm, f: Val<fn(i64) -> i64>, n: i64| -> Result<i64, Ctl> {
        if n == 0 { return Ok(0); }
        // The callee lives in rooted `state`, not in a capture.
        let (mut i, mut acc) = (0i64, 0i64);
        Err(Ctl::call(f.raw(), vec![Value::Int(0)], vec![f.raw()], move |_vm, r, state| {
            if let Value::Int(x) = r { acc += x; }
            i += 1;
            if i < n { Err(Ctl::call_again(state[0], vec![Value::Int(i)])) } else { Ok(Value::Int(acc)) }
        }))
    });
    // Sorts a list in place with a Sand comparator.
    m.vm_function("sort_with", |vm: &mut Vm, xs: Val<Vec<Param<0>>>, less: Val<fn(Param<0>, Param<0>) -> bool>| -> Result<(), Ctl> {
        let items = vm.heap.list(xs.raw()).to_vec();
        let n = items.len();
        let src = vm.heap.new_list(items.clone());
        let dst = vm.heap.new_list(items);
        let mut state = vec![xs.raw(), src, dst, less.raw()];
        let mut sort = MergeSort::new(n);
        if n > 1 { sort.start_run(); }
        match sort.advance(vm, &mut state, None) {
            None => Ok(()),
            Some((a, b)) => Err(Ctl::call(less.raw(), vec![a, b], state, move |vm, r, state| {
                match sort.advance(vm, state, Some(matches!(r, Value::Bool(true)))) {
                    None => Ok(Value::Unit),
                    Some((a, b)) => Err(Ctl::call_again(state[3], vec![a, b])),
                }
            })),
        }
    });
    m.vm_function("call", |_vm: &mut Vm, f: Val<fn() -> Param<0>>| -> Result<Param<0>, Ctl> {
        Err(Ctl::call(f.raw(), vec![], vec![], |_vm, r, _| Ok(r)))
    });
    m.vm_function("bounce", |_vm: &mut Vm, f: Val<fn(i64) -> i64>, d: i64| -> Result<i64, Ctl> {
        Err(Ctl::call(f.raw(), vec![Value::Int(d)], vec![], |_vm, r, _| Ok(r)))
    });
    // Calls f, then allocates a list of f() elements. The continuation checks
    // memory before it allocates, as SPEC §16.3 requires of natives.
    m.vm_function("effect_then_alloc", |_vm: &mut Vm, f: Val<fn() -> i64>| -> Result<Vec<i64>, Ctl> {
        Err(Ctl::call(f.raw(), vec![], vec![], |vm, r, _| {
            CONTINUATION_RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Value::Int(n) = r else { return Ok(Value::Unit) };
            vm.check_memory(n as usize * 16)?;
            Ok(vm.heap.new_list(vec![Value::Int(0); n as usize]))
        }))
    });
    // Calls f, then checks memory and wraps f's heap result in a new list.
    m.vm_function("wrap_result", |_vm: &mut Vm, f: Val<fn() -> Vec<String>>| -> Result<Val<Vec<Vec<String>>>, Ctl> {
        Err(Ctl::call(f.raw(), vec![], vec![], |vm, r, _| {
            vm.check_memory(64)?;
            Ok(vm.heap.new_list(vec![r]))
        }))
    });
    m.source("pub native fn wrap_result(f: fn() -> [str]) -> [[str]]\npub native fn effect_then_alloc(f: fn() -> int) -> [int]\npub native fn repeat_(f: fn(int) -> int, n: int) -> int\npub native fn sort_with<T>(xs: [T], less: fn(T, T) -> bool)\npub native fn call<T>(f: fn() -> T) -> T\npub native fn bounce(f: fn(int) -> int, d: int) -> int");
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
    println!("== sandlang prototype P2 (stackless continuations)");
    let limits = Limits { max_instructions: None, max_cpu_time: None, max_wall_time: None, ..Limits::default() };
    let config = |l: Limits| Config::new().host_modules(HostModules::new([module()]).unwrap()).limits(l);
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
    // E6: sort 200,000 ints; the same merge sort as a native with callbacks and in Sand; only the sort is timed
    s.eval(SORTBENCH).await.unwrap();
    let r = s.eval("\"native merge sort + Sand comparator (P2) \\(bench(xs => host.sort_with(xs, (a, b) => a < b))); merge sort written in Sand \\(bench(xs => merge_sort(xs, (a, b) => a < b))); sort_by(key) \\(bench(xs => xs.sort_by(x => x))); sort() \\(bench(xs => xs.sort()))\"").await;
    println!("E6 sort 200000 ints, median of 5: {:?}", r.map_err(|e| e.to_string()));
    for src in ["[3, 1, 2]", "[5, 4, 3, 2, 1]", "[2, 1]", "[1]"] {
        let r = s.eval(&format!("let t = {src}\nhost.sort_with(t, (a, b) => a < b)\nt")).await;
        println!("E6 small sort {src}: {r:?}");
    }
    let r = s.eval("let ws = [3, 1, 2]\nhost.sort_with(ws, (a, b) => { ws.push(0)\n a < b })\nws").await;
    println!("E6b comparator that mutates the list: {r:?}");
    // E3: a callback suspends on a timer and other tasks run meanwhile
    let r = s.eval("var ticks = 0\nlet bg = spawn { for _ in 0..3 { ticks += 1\n task.sleep(1ms) } }\nlet v = host.call(() => { task.sleep(10ms)\n ticks * 100 })\nbg.wait()\nv").await;
    println!("E3 callback that sleeps 10 ms while another task runs: {r:?}");
    // E2: native<->script recursion uses fiber frames, not the Rust stack
    for d in [1000, 5000, 9990, 10_000, 100_000] {
        let r = s.eval(&format!("fn f(d: int) -> int {{ if d >= {d} {{ return d }}\n host.bounce(f, d + 1) }}\nf(0)")).await;
        println!("E2 native<->script recursion to depth {d}: {:?}", r.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string()));
    }
    // E4: fuel and memory limits apply inside callbacks
    let mut s4 = Session::new(config(Limits { max_instructions: Some(10_000_000), ..limits.clone() }));
    let r = s4.eval("import host\nhost.call(() => { var i = 0\n while true { i += 1 }\n i })").await;
    println!("E4 instruction limit inside callback: {:?}", r.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string()));
    let mut s5 = Session::new(config(Limits { max_memory: Some(16 << 20), ..limits.clone() }));
    let r = s5.eval("import host\nhost.call(() => { var xs: [[int]] = []\n while true { xs.push([1, 2, 3]) }\n 0 })").await;
    println!("E5 memory limit inside callback: {:?}", r.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string()));
    // E11: GC during callbacks keeps continuation state alive
    let mut s6 = Session::new(Config::new().host_modules(HostModules::new([module()]).unwrap()).gc_stress(true).limits(limits.clone()));
    let r = s6.eval("import host\nlet xs = (0..300).map(i => (i * 7919) % 300)\nhost.sort_with(xs, (a, b) => { let junk = [a, b, a + b]\n junk[0] < junk[1] })\nxs == (0..300).map(i => i)").await;
    println!("E11 sort under gc_stress with allocating comparator: {r:?}");
    // E13: the engine re-runs a vm_function after a failed memory check (retry_stress fails the first check)
    let mut s7 = Session::new(Config::new().host_modules(HostModules::new([module()]).unwrap()).retry_stress(true).limits(limits.clone()));
    let r = s7.eval("import host\nvar calls = 0\nlet v = host.effect_then_alloc(() => { calls += 1\n 3 })\n\"callback ran \\(calls) times, native returned \\(v)\"").await;
    println!("E13 callback with a side effect, then a native step that checks memory and is retried: {:?}; the continuation ran {} times", r.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string()), CONTINUATION_RUNS.load(std::sync::atomic::Ordering::Relaxed));
    // E14: cancelling a task while its callback is suspended; the task's defer runs; the fiber is reused
    let r = s.eval("var log: [str] = []\nfn job() -> int {\n defer { log.push(\"deferred\") }\n host.call(() => { task.sleep(1s)\n 1 })\n}\nlet t = spawn job()\ntask.sleep(5ms)\nt.cancel()\nlet again = host.repeat_(x => x + 1, 3)\nlet t2 = spawn host.call(() => { task.sleep(1ms)\n 7 })\n\"\\(log) \\(again) \\(t2.wait())\"").await;
    println!("E14 cancel a task whose callback is suspended, then reuse fibers: {:?}", r.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string()));
    // E12: a comparator that empties the list and allocates, under gc_stress
    let r = s6.eval("let t = (0..64).map(i => [64 - i])\nhost.sort_with(t, (a, b) => { t.clear()\n var junk: [[int]] = []\n for i in 0..50 { junk.push([i]) }\n a[0] < b[0] })\nt.map(x => x[0]) == (1..65).map(i => i)").await;
    println!("E12 comparator that empties the list under gc_stress: {:?}", r.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string()));
    // E16: a synchronous host entry: poll an eval once; fail if it suspends
    use std::future::Future;
    use std::task::{Context, Poll, Waker};
    fn poll_once<F: Future>(f: F) -> Option<F::Output> {
        let mut f = std::pin::pin!(f);
        match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) { Poll::Ready(v) => Some(v), Poll::Pending => None }
    }
    let mut s8 = Session::new(config(limits.clone()));
    s8.eval("import host\nvar count = 0\nfn on_layout(w: int) -> int { count += 1\n w / 2 }\nfn on_fetch() -> int { task.sleep(5ms)\n 1 }").await.unwrap();
    let ready = poll_once(s8.eval("on_layout(640)"));
    let suspended = poll_once(s8.eval("on_fetch()"));
    let after = s8.eval("count").await;
    let n = 10_000;
    let t = Instant::now();
    for _ in 0..n { let _ = poll_once(s8.eval("on_layout(640)")); }
    let per = t.elapsed().as_secs_f64() / n as f64 * 1e6;
    println!("E16 sync entry by polling once: handler that returns {ready:?}; handler that sleeps {:?}; session afterwards {after:?}; {per:.1} us per sync entry (eval of a call expression)", suspended.map(|_| "ready"));
    // E13b: the retried step must keep the callback's heap result alive
    let mut s9 = Session::new(Config::new().host_modules(HostModules::new([module()]).unwrap()).retry_stress(true).gc_stress(true).limits(limits.clone()));
    let r = s9.eval("import host\nhost.wrap_result(() => [\"view\", \"tree\"])").await;
    println!("E13b heap result of a callback across a retried step, under retry_stress and gc_stress: {:?}", r.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string()));
    // E16c: a handler that only computes, for longer than the scheduler's 2 ms yield interval
    s8.eval("fn on_measure(n: int) -> int { var a = 0\n for i in 0..n { a += i % 7 }\n a }\nvar log: [int] = []\nfn on_change() -> int { log.push(1)\n task.sleep(1ms)\n log.len() }").await.unwrap();
    for n in [10_000, 100_000, 1_000_000] {
        let t = Instant::now();
        let r = poll_once(s8.eval(&format!("on_measure({n})")));
        println!("E16c compute-only handler on_measure({n}) by one poll: {:?} after {:.2} ms", r.map(|x| x.map_err(|e| e.to_string())), t.elapsed().as_secs_f64() * 1e3);
    }
    // E16d: a handler that changes state, then suspends: the sync entry fails, and the change stays
    let r = poll_once(s8.eval("on_change()"));
    let log = s8.eval("log").await;
    println!("E16d handler that changes state, then sleeps: {:?}; state afterwards: {log:?}", r.map(|_| "ready"));
}
