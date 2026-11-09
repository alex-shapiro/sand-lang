use callback_lab::{N, median_ns};
use gc_arena::Collect;
use piccolo::{
    BoxSequence, Callback, CallbackReturn, Closure, Context, Error, Execution, Executor, Fuel, Function, Lua,
    Sequence, SequencePoll, Stack, StaticError, Value,
};

/// native.repeat_(f, n): calls f(i) n times without recursing on the Rust stack.
#[derive(Collect)]
#[collect(no_drop)]
struct RepeatSeq<'gc> { f: Function<'gc>, n: i64, i: i64, acc: i64 }

impl<'gc> Sequence<'gc> for RepeatSeq<'gc> {
    fn poll(&mut self, _ctx: Context<'gc>, _exec: Execution<'gc, '_>, mut stack: Stack<'gc, '_>) -> Result<SequencePoll<'gc>, Error<'gc>> {
        if self.i > 0 {
            if let Value::Integer(v) = stack.get(0) { self.acc += v; }
        }
        stack.clear();
        if self.i < self.n {
            stack.push_back(Value::Integer(self.i));
            self.i += 1;
            Ok(SequencePoll::Call { function: self.f, is_tail: false })
        } else {
            stack.push_back(Value::Integer(self.acc));
            Ok(SequencePoll::Return)
        }
    }
}

/// Keeps a frame for the native while the callback runs (a non-tail call).
#[derive(Collect)]
#[collect(require_static)]
struct PassThrough;
impl<'gc> Sequence<'gc> for PassThrough {
    fn poll(&mut self, _ctx: Context<'gc>, _exec: Execution<'gc, '_>, _stack: Stack<'gc, '_>) -> Result<SequencePoll<'gc>, Error<'gc>> {
        Ok(SequencePoll::Return)
    }
}

fn setup(lua: &mut Lua) {
    lua.enter(|ctx| {
        let repeat = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let f: Function = stack.from_front(ctx)?;
            let n: i64 = stack.from_front(ctx)?;
            stack.clear();
            Ok(CallbackReturn::Sequence(BoxSequence::new(&ctx, RepeatSeq { f, n, i: 0, acc: 0 })))
        });
        ctx.set_global("repeat_", repeat).unwrap();
        let bounce = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let f: Function = stack.from_front(ctx)?;
            let d: i64 = stack.from_front(ctx)?;
            stack.clear();
            stack.push_back(Value::Integer(d));
            Ok(CallbackReturn::Call { function: f, then: Some(BoxSequence::new(&ctx, PassThrough)) })
        });
        ctx.set_global("bounce", bounce).unwrap();
        let call = Callback::from_fn(&ctx, |ctx, _, mut stack| {
            let f: Function = stack.from_front(ctx)?;
            stack.clear();
            Ok(CallbackReturn::Call { function: f, then: Some(BoxSequence::new(&ctx, PassThrough)) })
        });
        ctx.set_global("call_", call).unwrap();
    });
}

fn run(lua: &mut Lua, code: &str) -> Result<i64, StaticError> {
    let ex = lua.try_enter(|ctx| {
        let c = Closure::load(ctx, None, code.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, c.into(), ())))
    })?;
    lua.execute::<i64>(&ex)
}

fn main() {
    println!("== piccolo 0.3.3");
    let mut lua = Lua::full();
    setup(&mut lua);
    let n = N;
    let base = median_ns(5, n, || { run(&mut lua, &format!("local a = 0 for i = 0, {} do a = a + (i + 1) end return a", n - 1)).unwrap(); });
    let s2s = median_ns(5, n, || { run(&mut lua, &format!("local function inc(x) return x + 1 end local a = 0 for i = 0, {} do a = a + inc(i) end return a", n - 1)).unwrap(); });
    let n2s = median_ns(5, n, || { run(&mut lua, &format!("return repeat_(function(x) return x + 1 end, {n})")).unwrap(); });
    println!("E1 loop without call: {base:.1} ns/iter; script->script call loop: {s2s:.1} ns/iter; native->script callback loop: {n2s:.1} ns/iter");
    // E2: native<->script recursion does not use the Rust stack
    for depth in [10_000i64, 1_000_000] {
        let r = run(&mut lua, &format!("local function f(d) if d >= {depth} then return d end return bounce(f, d + 1) end return f(0)"));
        println!("E2 native<->script recursion to depth {depth}: {:?}", r.map_err(|e| e.to_string()));
    }
    // E3: a callback yields across the native frame
    let r = run(&mut lua, r#"
        local co = coroutine.create(function() return call_(function() local v = coroutine.yield("y") return v * 2 end) end)
        local ok1, y = coroutine.resume(co)
        local ok2, r = coroutine.resume(co, 21)
        assert(ok1 and y == "y" and ok2) return r"#);
    println!("E3 yield inside callback, resumed with 21: {:?}", r.map_err(|e| e.to_string()));
    // E4: the host drives execution in fuel slices, including inside callbacks
    let ex = lua.try_enter(|ctx| {
        let c = Closure::load(ctx, None, &b"return repeat_(function(x) while true do end end, 1)"[..])?;
        Ok(ctx.stash(Executor::start(ctx, c.into(), ())))
    }).unwrap();
    let mut slices = 0;
    let done = loop {
        let mut fuel = Fuel::with(10_000);
        if lua.enter(|ctx| ctx.fetch(&ex).step(ctx, &mut fuel)) { break true; }
        slices += 1;
        if slices == 1000 { break false; }
    };
    println!("E4 infinite loop inside callback: finished={done} after {slices} fuel slices of 10000; host regained control after each slice");
}
