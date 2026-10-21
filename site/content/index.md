+++
description = "Sand is a statically typed scripting language that runs untrusted code in a sandbox. It is alpha software."
+++

<div class="hero">
<div class="hero-text">

<h1 class="wordmark">Sand</h1>

<p class="tagline">A statically typed scripting language that runs untrusted code in a sandbox.</p>

<div class="actions">
<a class="button primary" href="/guide/">Read the guide</a>
<a class="button" href="/play/">Open the playground</a>
<a class="button" href="/std/">Library reference</a>
</div>

<div class="notice" role="note">

**Sand is alpha software.** The language, its library and its tools change without notice. Do not use Sand in production, and do not rely on its sandbox to protect anything of value yet.

This site documents Sand as it is today.

</div>
</div>
<div class="hero-logo"><img src="/logo.svg" alt="The Sand logo: a glass cube that holds sand" width="358" height="408"></div>
</div>

<section class="feature">
<div>

## No access without a policy

A Sand script cannot touch the outside world unless its host allows it. The engine checks every file, network, environment and standard stream access against a policy before the access happens. A denied I/O function returns an ordinary error value that the script can handle.

The host also sets hard limits on memory, CPU time, instructions and output for each engine. Sand has no FFI, no subprocesses and no JIT.

</div>
<div>

```sand
import http

match http.get("https://api.example.com/v1/status") {
    Ok(response) => print("status \(response.status)")
    Err(e) => print("no network: \(e)")
}
```

```policy
allow read ./data
allow http GET api.example.com/v1/**
limit memory 64M
limit time 10s
```

</div>
</section>

<section class="feature">
<div>

## Mistakes stop at compile time

Sand checks types before a script runs. Function signatures carry types, and Sand infers the rest. There is no null: a value that can be absent has an `Option` type. A `match` must handle every case.

</div>
<div>

```sand error
enum Shape {
    Circle(radius: float)
    Rect(w: float, h: float)
    Empty
}

fn area(s: Shape) -> float {
    match s {
        Circle(r) => 3.14159 * r * r
        Rect(w, h) => w * h
    }
}
```

</div>
</section>

<section class="feature">
<div>

## Small, expression-oriented syntax

Blocks, `if` and `match` are expressions. Strings interpolate values with format specifications. Lists, maps and sets have comprehensions and a full set of methods, and lazy iterators handle long or endless sequences.

</div>
<div>

```sand
struct Planet {
    name: str
    moons: int
}

let planets = [
    Planet { name: "Mercury", moons: 0 },
    Planet { name: "Earth", moons: 1 },
    Planet { name: "Jupiter", moons: 95 },
]

for p in planets.sorted_by(p => -p.moons) {
    let count = if p.moons == 1 { "1 moon" } else { "\(p.moons) moons" }
    print("\(p.name:<8) \(count)")
}
print("total: \(planets.map(p => p.moons).sum())")
```

</div>
</section>

<section class="feature">
<div>

## Concurrency without colored functions

All I/O is asynchronous, but you do not mark functions `async`. `spawn` starts a task, and any function can wait for a task, a channel or a timer. The engine runs one task at a time on one thread, so several engines can share a process safely.

</div>
<div>

```sand
fn slow_square(n: int) -> int {
    task.sleep(10ms * (5 - n))
    n * n
}

let tasks = [spawn slow_square(n) for n in 1..5]
print(task.all(tasks))
```

</div>
</section>

<section class="feature">
<div>

## Embedded in Rust

Sand is a Rust library. A host program creates engines, gives each one a policy and limits, and adds native modules with typed functions. Sand compiles to WebAssembly too: the [playground](/play/) runs it in your browser.

</div>
<div>

```rust
use sandlang::{Config, Engine, NativeModule, Policy};

let mut geo = NativeModule::new("geo");
geo.function("dist", |a: f64, b: f64| -> f64 { (a * a + b * b).sqrt() });

let config = Config::new().policy(Policy::deny_all()).module(geo);
let mut engine = Engine::new(config);
engine.run_source("main.sand", "import geo\nprint(geo.dist(3.0, 4.0))").await?;
```

</div>
</section>

<section class="grid3">
<div>

### Tools built in

`sand` checks, lints, formats and tests scripts, and has a REPL. [Tools](/guide/tools/)

</div>
<div>

### Safe Rust, no JIT

The compiler and the bytecode VM are 100% safe Rust. Static types let the compiler emit specialized instructions.

</div>
<div>

### Documented as it is

The [specification](/spec/) is normative. The site's build checks the Sand examples on this site and shows what they printed.

</div>
</section>
