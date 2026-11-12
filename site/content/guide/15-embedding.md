+++
title = "Embedding"
description = "Embed in a Rust application or web page."
+++

You can embed a Sand compiler and VM in a larger Rust application that runs natively or in a webassembly module.

## Running a script

`Engine::run_source` compiles and executes a script inside a future. It runs on any Tokio runtime with timers enabled:

```rust
use sandlang::engine::{Config, Engine, Error, Outcome};

async fn run(source: &str) -> Result<Outcome, Error> {
    Engine::new(Config::new()).run_source("main.sand", source).await
}

#[tokio::main]
async fn main() {
    match run(r#"print("hello from Sand")"#).await {
        Ok(outcome) => println!("exit code {}", outcome.exit_code),
        Err(err) => eprint!("{}", err.render()),
    }
}
```

Use `Engine::run_file` to load a script from disk instead. If compilation or execution fails, `err.render()` formats the error with source snippets and stack traces.

Each `Engine` runs independently. You can run multiple engines in parallel across threads or tasks.

To run the same script many times, compile it once with `Engine::compile_source` or `Engine::compile_file`. Pass the resulting `CompiledProgram` to `Engine::run_program`. Each run starts with fresh globals:

```rust
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
use sandlang::engine::{Config, Engine};

let program = Engine::new(Config::new()).compile_source("main.sand", "print(\"hello\")").await?;
for _ in 0..3 {
    Engine::new(Config::new()).run_program(&program).await?;
}
# Ok(())
# }
```

## Configuration

By default, `Config::new()` allows scripts to write to `stdout` and `stderr` and denies all other capabilities. Use builder methods on `Config` to grant permissions, set resource limits, and replace host I/O with in-memory files and streams:

```rust
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
use sandlang::engine::{Config, Engine, Limits, MemorySink};
use sandlang::files::mem::MemFs;
use sandlang::policy::rules::RulePolicy;
use std::time::Duration;

let files = MemFs::new();
files.write("/data/in.txt", "3 1 2")?;
files.create_dir_all("/out")?;
let out = MemorySink::new();
let script = r#"
import fs

let words = fs.read_text("/data/in.txt")?.split(" ")
fs.write_text("/out/report.txt", words.sorted().join(","))?
print("sorted \(words.len()) words")
"#;

let config = Config::new()
    .policy(RulePolicy::standard().allow_read("/data".parse()?).allow_write("/out".parse()?))
    .limits(Limits {
        max_memory: Some(64 << 20),
        max_wall_time: Some(Duration::from_secs(5)),
        ..Limits::default()
    })
    .memory_fs(files.clone())
    .stdout(out.clone());

let mut engine = Engine::new(config);
engine.run_source("main.sand", script).await?;
println!("{}", out.contents());
let report = files.read("/out/report.txt")?;
# Ok(())
# }
```

The full set of capabilities and limits can be found in [Sandbox and policies](/guide/sandbox/).

You can also load rules and limits from a `.policy` file with `PolicyFile::parse`:

```rust no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use sandlang::engine::{Config, Limits};
# use sandlang::policy::rules::RulePolicy;
use sandlang::policy::file::PolicyFile;
use std::path::Path;

let text = std::fs::read_to_string("app.policy")?;
let file = PolicyFile::parse(&text, Path::new("."), None).expect("a valid policy file");
let limits = file.apply_limits(Limits::default());
let config = Config::new().policy(file.rules.with_defaults(RulePolicy::standard())).limits(limits);
# Ok(())
# }
```

## Native modules

Native modules expose Rust functions and types as Sand modules. The Sandlang standard library is a collection of native modules, and you can register your own:

```rust
# fn main() -> Result<(), Box<dyn std::error::Error>> {
use sandlang::engine::{Config, HostModules};
use sandlang::native::NativeModule;
use sandlang::native::convert::HostError;

let mut geo = NativeModule::new("geo");
geo.function("dist", |x: f64, y: f64| -> f64 { (x * x + y * y).sqrt() })
    .params(&["x", "y"])
    .doc("Returns the distance of (x, y) from the origin.");
geo.function("country", |code: &str| -> Result<String, HostError> {
    match code {
        "NL" => Ok("Netherlands".to_string()),
        _ => Err(HostError::new("not_found", format!("unknown country {code}"))),
    }
});
let config = Config::new().host_modules(HostModules::new([geo])?);
# Ok(())
# }
```

A Sand script can then import and call the module:

```sand ignore
import geo

print(geo.dist(3.0, 4.0))
print(geo.country("XX"))
```

Running the script above prints `5.0` and `Err(Error(message: "unknown country XX", kind: "not_found"))`.

Native function signatures are inferred from their Rust parameter and return types:

| Rust type                                                 | Sand type                    |
| --------------------------------------------------------- | ---------------------------- |
| `i64`, `f64`, `bool`, `()`                                | `int`, `float`, `bool`, `()` |
| `String`, `&str`                                          | `str`                        |
| `Vec<u8>`, `&[u8]`                                        | `bytes`                      |
| `Vec<T>`                                                  | `[T]`                        |
| `Option<T>`                                               | `T?`                         |
| `Result<T, E>`                                            | `Result<T, Error>`           |
| `HashMap<K, V>`, `BTreeMap<K, V>`                         | `[K: V]`                     |
| `HashSet<T>`, `BTreeSet<T>`                               | `Set<T>`                     |
| tuples of up to 8 elements                                | tuples                       |
| Rust struct with `#[derive(SandStruct)]`                  | matching Sand `struct`       |
| Rust enum with `#[derive(SandEnum)]`                      | matching Sand `enum`         |
| `&T` parameter or `T` return with `#[derive(SandObject)]` | opaque type `T`              |

A native function may be `async` as long as its parameter and return types are `Send`.

### Custom types

You can expose a Rust struct to Sand with the `SandStruct` macro. Set the struct's module name with `#[sand(module = "...")]` and register the type with `NativeModule::bind`. You can also register type methods:

```rust
use sandlang::SandStruct;
use sandlang::native::NativeModule;

/// A point in the plane.
#[derive(SandStruct)]
#[sand(module = "geo")]
struct Point {
    x: f64,
    #[sand(default)]
    y: f64,
}

let mut geo = NativeModule::new("geo");
geo.bind::<Point>();
geo.function("mid", |a: Point, b: Point| Point { x: (a.x + b.x) / 2.0, y: (a.y + b.y) / 2.0 });
geo.function("Point.norm", |p: Point| (p.x * p.x + p.y * p.y).sqrt());
```

Struct fields are public and immutable by default. Add `#[sand(default)]` to give a field a default value.

Derive `SandEnum` to map unit, tuple, and struct variants to a Sand `enum`:

```rust
use sandlang::SandEnum;
use sandlang::native::NativeModule;

/// A two-dimensional shape.
#[derive(SandEnum)]
#[sand(module = "geo")]
enum Shape {
    Circle(f64),
    Rect { w: f64, h: f64 },
    Empty,
}

let mut geo = NativeModule::new("geo");
geo.bind::<Shape>();
geo.function("area", |s: Shape| match s {
    Shape::Circle(r) => std::f64::consts::PI * r * r,
    Shape::Rect { w, h } => w * h,
    Shape::Empty => 0.0,
});
```

To expose an opaque type to Sand, derive `SandObject` and implement `NativeObject`.

```rust
use sandlang::SandObject;
use sandlang::native::{NativeModule, NativeObject};
use std::sync::atomic::{AtomicI64, Ordering};

/// A shared atomic counter.
#[derive(SandObject)]
#[sand(module = "stats")]
struct Counter(AtomicI64);

impl NativeObject for Counter {}

let mut stats = NativeModule::new("stats");
stats.bind::<Counter>();
stats.function("counter", || Counter(AtomicI64::new(0)));
stats.function("Counter.add", |c: &Counter, n: i64| c.0.fetch_add(n, Ordering::Relaxed) + n).params(&["n"]);
```

### Checking permissions

To check a sandbox capability inside a native function, take `&mut Vm` as the first parameter and call `Vm::require` before performing any side effect. `Vm::require` returns a `"permission_denied"` error if the policy denies the request:

```rust
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
use sandlang::engine::{Config, Engine, HostModules};
use sandlang::native::NativeModule;
use sandlang::native::convert::HostError;
use sandlang::policy::Capability;
use sandlang::policy::rules::{CustomMatch, RulePolicy};
use sandlang::vm::Vm;

let mut door = NativeModule::new("door");
door.function("open", |vm: &mut Vm, name: String| -> Result<String, HostError> {
    vm.require(&Capability::Custom { name: "door".into(), detail: name.clone() })?;
    Ok(format!("opened {name}"))
});
let policy = RulePolicy::standard().allow_custom(CustomMatch::Exact("door".into()), CustomMatch::Exact("front".into()));
let mut engine = Engine::new(Config::new().host_modules(HostModules::new([door])?).policy(policy));
engine.run_source("main.sand", "import door\nprint(door.open(\"front\"))\nprint(door.open(\"back\"))").await?;
# Ok(())
# }
```

Running the script above prints:

```text
Ok("opened front")
Err(Error(message: "permission denied: door back", kind: "permission_denied"))
```

## Interactive sessions

`Session::new` creates an interactive, REPL-like `Session` that evaluates inputs one at a time and maintains state across calls:

```rust
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# use sandlang::engine::Config;
# use sandlang::session::Session;
# let config = Config::new();
let mut session = Session::new(config);
session.eval("var hits = 0\nfn hit() -> int {\n    hits += 1\n    hits\n}").await?;
assert_eq!(session.eval("hit() + hit()").await?, Some("3".to_string()));
# Ok(())
# }
```

`Session::eval` returns the formatted value of a trailing expression, or `None` for statements and declarations. Call `Session::reset` to clear session state.

## Interrupts

Use an `InterruptHandle` to cancel a running script from another thread. Pass a clone of the handle to `Config::interrupt` before creating the engine or session:

```rust
# #[tokio::main]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
use sandlang::engine::{Config, Engine, Error, InterruptHandle};
use std::time::Duration;

let handle = InterruptHandle::new();
let mut engine = Engine::new(Config::new().interrupt(handle.clone()));
std::thread::spawn(move || {
    std::thread::sleep(Duration::from_secs(1));
    handle.interrupt();
});
let result = engine.run_source("main.sand", "while true {}").await;
assert!(matches!(result, Err(Error::Interrupted)));
# Ok(())
# }
```

## WebAssembly

Run Sand as a WASM component with the `sandlang-wasm` crate, which wraps a WASM build in a JavaScript API:

```js
import init, { Engine } from "./pkg/sandlang_wasm.js";

await init();
const engine = new Engine(
  { policy: "allow /data", files: { "/data/in.txt": "hello" } },
  { onOutput: (stream, text) => console.log(stream, text) },
);
const result = await engine.run(
  'import fs\nprint(fs.read_text("/data/in.txt")?)',
);
console.log(result.ok, result.exitCode);
```

Run an `Engine` inside a Web Worker to keep the browser UI thread responsive. For an example, see the code for the sandlang web playground. For a complete Rust embedding reference, see [§16 of the specification](/spec/#16-embedding-api-rust).
