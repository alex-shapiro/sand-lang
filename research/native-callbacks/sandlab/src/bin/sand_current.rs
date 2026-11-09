//! Sand today: per-call costs, and the deferred-callback workaround (`Vm::spawn_task`).
use sandlang::engine::{Config, HostModules, Limits};
use sandlang::native::NativeModule;
use sandlang::native::ty::{Task, Val};
use sandlang::session::Session;
use sandlang::vm::{Ctl, Vm};
use std::sync::Mutex;
use std::time::Instant;

static STORED: Mutex<Option<sandlang::vm::value::Value>> = Mutex::new(None);

fn module() -> NativeModule {
    let mut m = NativeModule::new("host");
    // Schedules a Sand closure as a new task: the only way a native can run Sand code today.
    m.vm_function("defer_", |vm: &mut Vm, f: Val<fn() -> i64>| -> Result<Val<Task<i64>>, Ctl> {
        Ok(Val::from_raw(vm.spawn_task(f.raw())?))
    });
    // Keeps a handle past the native call (the docs forbid this; E-S3 shows why).
    m.vm_function("keep", |_vm: &mut Vm, f: Val<fn() -> i64>| -> Result<(), Ctl> {
        *STORED.lock().unwrap() = Some(f.raw());
        Ok(())
    });
    m.vm_function("fire_kept", |vm: &mut Vm| -> Result<Val<Task<i64>>, Ctl> {
        let v = STORED.lock().unwrap().unwrap();
        Ok(Val::from_raw(vm.spawn_task(v)?))
    });
    m
}

async fn timed(s: &mut Session, src: &str) -> (f64, Option<String>) {
    let t = Instant::now();
    let r = s.eval(src).await;
    (t.elapsed().as_secs_f64(), r.unwrap_or_else(|e| Some(format!("ERROR {e}"))))
}

fn median(mut v: Vec<f64>) -> f64 { v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[v.len() / 2] }

#[tokio::main(flavor = "current_thread")]
async fn main() {
    println!("== sandlang (this branch), release build");
    let config = || Config::new().host_modules(HostModules::new([module()]).unwrap()).limits(Limits { max_instructions: None, max_cpu_time: None, max_wall_time: None, ..Limits::default() });
    let mut s = Session::new(config());
    let n = 5_000_000u64;
    s.eval("fn inc(x: int) -> int { x + 1 }").await.unwrap();
    s.eval("let inc_value: fn(int) -> int = x => x + 1").await.unwrap();
    let (mut b, mut c, mut v) = (vec![], vec![], vec![]);
    for _ in 0..5 {
        v.push(timed(&mut s, &format!("var a = 0\nfor i in 0..{n} {{ a += inc_value(i) }}\na")).await.0 / n as f64 * 1e9);
        b.push(timed(&mut s, &format!("var a = 0\nfor i in 0..{n} {{ a += i + 1 }}\na")).await.0 / n as f64 * 1e9);
        c.push(timed(&mut s, &format!("var a = 0\nfor i in 0..{n} {{ a += inc(i) }}\na")).await.0 / n as f64 * 1e9);
    }
    println!("E1 loop without call: {:.1} ns/iter; script->script direct call loop: {:.1} ns/iter; script->script call through fn value: {:.1} ns/iter; native->script callback: not possible", median(b), median(c), median(v));
    // E-S1: deferred callback through a task
    let m = 200_000u64;
    let mut d = vec![];
    for _ in 0..5 {
        d.push(timed(&mut s, &format!("import host\nvar a = 0\nfor i in 0..{m} {{ a += host.defer_(() => i + 1).wait() }}\na")).await.0 / m as f64 * 1e9);
    }
    let r = s.eval(&format!("import host\nvar a = 0\nfor i in 0..10 {{ a += host.defer_(() => i + 1).wait() }}\na")).await;
    println!("E-S1 native schedules Sand closure as task, script waits: result {:?}; {:.1} ns per round trip", r, median(d));
    // E-S1b: a deferred callback may suspend on I/O (async)
    let r = s.eval("import host\nhost.defer_(() => { task.sleep(1ms)\n 7 }).wait()").await;
    println!("E-S1b deferred callback that sleeps: {:?}", r);
    // E-S3: a handle kept past the native call is not a GC root
    let mut s2 = Session::new(config());
    let r = s2.eval("import host\nlet k = 41\nhost.keep(() => k + 1)").await;
    println!("E-S3 keep: {:?}", r);
    let r = s2.eval("var junk: [[int]] = []\nfor i in 0..200000 { junk.push([i, i]) }\njunk = []\nfor i in 0..200000 { junk.push([i]) }\n0").await;
    println!("E-S3 churn heap: {:?}", r.map(|_| "ok"));
    let r = s2.eval("import host\nhost.fire_kept().wait()").await;
    println!("E-S3 fire kept handle after GC: {:?}", r);
}
