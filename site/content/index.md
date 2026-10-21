+++
description = "Sand is a statically typed scripting language that runs untrusted code, safely."
+++

<div class="hero">
<div class="hero-text">

<h1 class="wordmark">Sand</h1>

<p class="tagline">An interpreter for running untrusted code, safely.</p>

<div class="actions">
<a class="button primary" href="/guide/">Read the guide</a>
<a class="button" href="/play/">Open the playground</a>
<a class="button" href="/std/">Library reference</a>
</div>

<div class="notice" role="note">

**Sand is alpha software.** The language, standard library, and toolchain are subject to change change without notice.

</div>
</div>
<div class="hero-logo"><img src="/logo.svg" alt="The Sand logo: a glass cube that holds sand" width="358" height="408"></div>
</div>

<section class="feature">
<div>

## Capability-based sandboxing

The sand interpreter is sandboxed by default. Operations that touch outside resources, like file and network I/O, must be checked against a security policy before they execute. The interpeter pauses during each check to prevent untrusted code from taking unwanted actions.

Policies also enforce limits on memory and compute.

</div>
<div>

```sand fails
import http
let result = http.get("https://example.com/v1/status")?
print(result)
```

```policy
allow http sandlang.com
limit memory 8M
limit time 5s
```

</div>
</section>

<section class="feature">
<div>

## Expressive Types

Sand is statically typed. It includes first-class primitives for pattern-matching, error handling, and null handling. Users can define custom types and traits, and the compiler points out type errors with helpful messages.

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

## Lightweight concurrency

Sand I/O is asynchronous. Any function can wait on tasks, channels, or timers. Concurrent tasks execute cooperatively on a single thread.

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

## Rust Interoperability

Sand is designed to be embedded in larger Rust programs. Multiple interpreters can run concurrently in a single process, each with its own policy.

Sand also runs in WebAssembly. See it in action in the [playground](/play/).

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

### Built-in tooling

Sand includes a type checker, linter, formatter, test runner, and interactive REPL. All tools are accessible from both the CLI and from the Rust library. [Tools](/guide/tools/)

</div>
<div>
