use callback_lab::{N, median_ns};
use mlua::{Function, Lua, UserData, UserDataMethods};

struct Counter { n: i64 }
impl UserData for Counter {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        // Holds &mut self while the callback runs.
        m.add_method_mut("each", |_, this, f: Function| { this.n += 1; f.call::<()>(()) });
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> mlua::Result<()> {
    println!("== mlua 0.11.6 (Lua 5.4, vendored)");
    let lua = Lua::new();
    lua.globals().set("repeat_", lua.create_function(|_, (f, n): (Function, i64)| {
        let mut acc = 0i64;
        for i in 0..n { acc += f.call::<i64>(i)?; }
        Ok(acc)
    })?)?;
    let n = N as i64;
    let loop0: Function = lua.load("return function(n) local a = 0 for i = 0, n - 1 do a = a + (i + 1) end return a end").eval()?;
    let loopc: Function = lua.load("local function inc(x) return x + 1 end return function(n) local a = 0 for i = 0, n - 1 do a = a + inc(i) end return a end").eval()?;
    let loopn: Function = lua.load("return function(n) return repeat_(function(x) return x + 1 end, n) end").eval()?;
    let b = median_ns(5, N, || { loop0.call::<i64>(n).unwrap(); });
    let s = median_ns(5, N, || { loopc.call::<i64>(n).unwrap(); });
    let c = median_ns(5, N, || { loopn.call::<i64>(n).unwrap(); });
    println!("E1 loop without call: {b:.1} ns/iter; script->script call loop: {s:.1} ns/iter; native->script callback loop (each Rust->Lua call is protected): {c:.1} ns/iter");
    // E9: a FnMut callback re-entered by the script it calls
    let reent = lua.create_function_mut(|_, f: Function| f.call::<()>(()))?;
    lua.globals().set("reent", reent)?;
    let r = lua.load("reent(function() reent(function() end) end)").exec();
    println!("E9 re-entered FnMut callback: {:?}", r.err().map(|e| e.to_string().lines().filter(|l| l.contains("recursiv") || l.contains("borrow")).collect::<Vec<_>>().join(" / ")));
    // E9: a userdata method that holds &mut self while the script calls the same object
    lua.globals().set("counter", Counter { n: 0 })?;
    let r = lua.load("counter:each(function() counter:each(function() end) end)").exec();
    println!("E9 userdata re-borrow during callback: {:?}", r.err().map(|e| e.to_string().lines().filter(|l| l.contains("recursiv") || l.contains("borrow")).collect::<Vec<_>>().join(" / ")));
    // E3: an async Rust function suspends Lua only inside a coroutine
    let sleep = lua.create_async_function(|_, ms: u64| async move {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        Ok("slept")
    })?;
    lua.globals().set("sleep", sleep)?;
    let f: Function = lua.load("return function() return repeat_(function(x) sleep(1) return 0 end, 1) end").eval()?;
    println!("E3 async host fn inside callback inside sync native: {:?}", f.call_async::<i64>(()).await.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string()));
    let g: Function = lua.load("return function() return sleep(1) end").eval()?;
    println!("E3 async host fn called directly from script: {:?}", g.call_async::<String>(()).await.map_err(|e| e.to_string()));
    Ok(())
}
