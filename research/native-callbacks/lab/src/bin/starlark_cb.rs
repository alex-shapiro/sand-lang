use callback_lab::{N, median_ns};
use starlark::environment::{Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::Value;
use starlark_derive::starlark_module;
use std::sync::atomic::{AtomicI64, Ordering};

static DEPTH: AtomicI64 = AtomicI64::new(0);

#[starlark_module]
fn natives(builder: &mut GlobalsBuilder) {
    fn repeat_<'v>(f: Value<'v>, n: i32, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<i32> {
        let mut acc = 0i32;
        for i in 0..n {
            let r = eval.eval_function(f, &[eval.heap().alloc(i)], &[]).map_err(|e| anyhow::anyhow!("{e}"))?;
            acc = acc.wrapping_add(r.unpack_i32().unwrap());
        }
        Ok(acc)
    }
    fn bounce<'v>(f: Value<'v>, d: i32, eval: &mut Evaluator<'v, '_, '_>) -> anyhow::Result<Value<'v>> {
        DEPTH.store(d as i64, Ordering::Relaxed);
        eval.eval_function(f, &[eval.heap().alloc(d)], &[]).map_err(|e| anyhow::anyhow!("{e}"))
    }
}

fn run(globals: &Globals, src: &str) -> Result<String, String> {
    let mut dialect = Dialect::Extended;
    dialect.enable_top_level_stmt = true;
    let ast = AstModule::parse("t.star", src.to_owned(), &dialect).map_err(|e| e.to_string())?;
    Module::with_temp_heap(|module| {
        let mut eval = Evaluator::new(&module);
        eval.eval_module(ast, globals).map(|v| v.to_string()).map_err(|e| format!("{e}").lines().filter(|l| l.contains("overflow")).collect::<Vec<_>>().join(" / "))
    })
}

fn main() {
    println!("== starlark-rust 0.14.2");
    let globals = GlobalsBuilder::standard().with(natives).build();
    let n = N / 10;
    let b = median_ns(5, n, || { run(&globals, &format!("def m():\n  a = 0\n  for i in range({n}):\n    a += i + 1\n  return a\nm()")).unwrap(); });
    let s = median_ns(5, n, || { run(&globals, &format!("def inc(x):\n  return x + 1\ndef m():\n  a = 0\n  for i in range({n}):\n    a += inc(i)\n  return a\nm()")).unwrap(); });
    let c = median_ns(5, n, || { run(&globals, &format!("def inc(x):\n  return x + 1\nrepeat_(inc, {n})")).unwrap(); });
    println!("E1 loop without call: {b:.1} ns/iter; script->script call loop: {s:.1} ns/iter; native->script callback loop: {c:.1} ns/iter");
    let r = run(&globals, "def f(d):\n  return bounce(f, d + 1)\nf(0)");
    println!("E2 native<->script recursion: depth {} | {:?}", DEPTH.load(Ordering::Relaxed), r);
}
