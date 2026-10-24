+++
title = "Embedding"
description = "Embed in a Rust application or web page."
+++

The `sandlang` crate embeds the Sand compiler and VM as a Rust library. Construct a `Config`, create an `Engine`, and run scripts under the configured sandbox policy and resource limits.

## Running a script

`Engine::run_source` compiles and executes a script, returning a future that runs on any Tokio runtime with timers enabled:

```rust
use sandlang::{Config, Engine, Error, Outcome};

async fn run(source: &str) -> Result<Outcome, Error> {
    Engine::new(Config::new())?.run_source("main.sand", source).await
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    match run(r#"print("hello from Sand")"#).await {
        Ok(outcome) => println!("exit code {}", outcome.exit_code),
        Err(err) => eprint!("{}", err.render()),
    }
}
```

The script executes inside the future on the polling thread. The first argument is the source name used in diagnostics and stack traces. Use `Engine::run_file(path)` to load the entry module from disk.

Failed runs return a `sandlang::Error`:

| Variant                           | Cause                                      |
| --------------------------------- | ------------------------------------------ |
| `Error::Compile(diagnostics)`     | syntax or static type errors               |
| `Error::Runtime { message, .. }`  | uncaught runtime failure                   |
| `Error::Limit(kind)`              | resource limit exceeded                    |
| `Error::Interrupted`              | execution cancelled via `InterruptHandle`  |
| `Error::Io(message)`              | entry script file could not be read        |
| `Error::Internal { message, .. }` | internal engine or native module invariant |

`err.render()` formats the error with source snippets and stack traces. `Engine::check_source` type-checks a script without executing it, and `Engine::lint_source` returns lint diagnostics.

## Configuration

`Config::new()` denies all host capabilities except `stdout`, `stderr`, and imports within the module root. Use builder methods on `Config` to configure policies, limits, virtual files, and streams:

```rust
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
use sandlang::{Config, Engine, Limits, MemFs, MemorySink, Policy, RulePolicy};
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
    .policy(Policy::rules(RulePolicy::new().allow_read("/data").allow_write("/out")))
    .limits(Limits {
        max_memory: Some(64 << 20),
        max_wall_time: Some(Duration::from_secs(5)),
        ..Limits::default()
    })
    .memory_fs(files.clone())
    .stdout(out.clone());

let mut engine = Engine::new(config)?;
engine.run_source("main.sand", script).await?;
println!("{}", out.contents());
let report = files.read("/out/report.txt")?;
# Ok(())
# }
```

- **Policy.** `RulePolicy` provides builder methods for each policy rule (`allow_read`, `allow_write`, `allow_net`, `allow_http`, `allow_env`, `deny_read`, `deny_net`, `fallback`, and others). `Policy::allow_all()` grants all capabilities and should be used only for trusted scripts.
- **Limits.** `Limits` configures every resource bound described in [Sandbox and policies](/guide/sandbox/).
- **Virtual file system.** `Config::memory_fs` replaces host disk access with an in-memory `MemFs`. Clones of a `MemFs` share the same underlying storage so the host can populate input files before a run and read output files afterward.
- **Streams.** `Config::stdout` and `Config::stderr` redirect script output to a `MemorySink` or custom sink. `Config::stdin_bytes` supplies fixed input bytes.
- **Logging.** `Config::log_sink` routes `log` records to a host `LogSink` as a `LogRecord` containing the level, message, fields, and caller location. `LogSink::enabled` filters levels before record construction, and `Config::log_level` sets the initial script log level. Emitted records count against the output limit.
- **Environment.** `Config::args` and `Config::env` set the command-line arguments and environment map exposed to the script. If `Config::env` is not set, `env.get` reads the host process environment subject to the policy.

To load a `.policy` file from disk, parse it with `sandlang::policy::PolicyFile`:

```rust no_run
# fn main() -> Result<(), Box<dyn std::error::Error>> {
# use sandlang::Config;
use sandlang::policy::PolicyFile;
use std::path::Path;

let text = std::fs::read_to_string("app.policy")?;
let file = PolicyFile::parse(&text, Path::new("."), None).expect("a valid policy file");
let config = Config::new().policy(file.policy()).limits(file.limits.clone());
# Ok(())
# }
```

The second argument is the base directory for `./` paths, and the third argument is the optional home directory for `~/` paths.

To handle `prompt` policy decisions, pass an async callback to `Config::prompter` that accepts a `Capability` and returns a `PromptAnswer`. If a prompter is not configured, `prompt` decisions default to `deny`.

## Native modules

`NativeModule` exposes Rust functions to Sand scripts as an importable module:

```rust
use sandlang::{Config, HostError, NativeModule};

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
let config = Config::new().module(geo);
```

```sand ignore
import geo

print(geo.dist(3.0, 4.0))
print(geo.country("XX"))
```

Running the script above prints `5.0` and `Err(Error { kind: "not_found", message: "unknown country XX" })`.

Sand infers the Sand function signature from the Rust parameter and return types:

| Rust type                                                    | Sand type                    |
| ------------------------------------------------------------ | ---------------------------- |
| `i64`, `f64`, `bool`, `()`                                   | `int`, `float`, `bool`, `()` |
| `String`, `&str`                                             | `str`                        |
| `Vec<u8>`, `&[u8]`                                           | `bytes`                      |
| `Vec<T>`                                                     | `[T]`                        |
| `Option<T>`                                                  | `T?`                         |
| `Box<T>`                                                     | `T`                          |
| `Result<T, E>`                                               | `Result<T, Error>`           |
| `HashMap<K, V>`, `BTreeMap<K, V>`, `IndexMap<K, V>`          | `[K: V]`                     |
| `HashSet<T>`, `BTreeSet<T>`                                  | `Set<T>`                     |
| `jiff::SignedDuration`                                       | `duration`                   |
| `sandlang::json::Json`                                       | `json.Json`                  |
| tuples of up to 8 elements                                   | tuples                       |
| Rust enum with `#[derive(SandEnum)]`                         | matching Sand `enum`         |
| Rust struct with `#[derive(SandStruct)]`                     | matching Sand `struct`       |
| `&T` (argument) or `T` (result) with `#[derive(SandObject)]` | opaque type `T`              |
| `ValueType` `T` with `#[derive(SandValue)]`                  | opaque value type `T`        |

The engine automatically exports registered functions that are not declared in `NativeModule::source` as `pub`, with default parameter names `arg1`, `arg2`, ... unless overridden by `params`. Registering a function as `"Type.name"` defines a method on `Type`; the method takes `self` if its first parameter has type `Type` (such as `Counter.add` below). Generic functions must be declared with `native fn` in `NativeModule::source`.

Returning `Err(HostError::new(kind, message))` produces a Sand `Err(Error)` with the given `kind` and `message`. The engine also converts `std::io::Error` into an `Error` with its standard kind, and `String` or `&str` errors into an `Error` with kind `"error"`. A function that returns `Result<T, sandlang::vm::Ctl>` has the Sand return type `T`; returning `Err(ctl)` aborts the run with a runtime error (`Ctl::fail`), a resource limit error, or an exit code.

Each derive macro (`SandEnum`, `SandStruct`, `SandObject`, `SandValue`) binds a Rust type to a Sand type in the module specified by `#[sand(module = "...")]`. Register each bound type with `NativeModule::bind`. Do not redeclare a bound type in `NativeModule::source`; use `extend` to add Sand methods to it. Optional type attributes override the Sand name (`#[sand(name = "Kind")]`), make the type private to its module (`#[sand(private)]`), or specify a renamed `sandlang` dependency (`#[sand(crate = my_sandlang)]`).

Derive `SandEnum` on a Rust enum to pass enum values between Rust and Sand:

```rust
use sandlang::{NativeModule, SandEnum};

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

The engine generates the `pub enum Shape` declaration in `geo` from the Rust variants and doc comments, so scripts can call `geo.area(geo.Shape.Rect(w: 2.0, h: 3.0))`.

Derive `SandStruct` on a Rust struct with named fields to copy struct values between Rust and Sand. Fields are `pub` and immutable by default; annotate a field with `#[sand(private)]` to make it private, `#[sand(var)]` to make it mutable, `#[sand(name = "kind")]` to override its Sand name, or `#[sand(default)]` (`#[sand(default = 1.0)]`) to give it a default value:

```rust
use sandlang::{NativeModule, SandStruct};

/// A point in the plane.
#[derive(SandStruct)]
#[sand(module = "geo")]
struct Point {
    /// Horizontal coordinate.
    x: f64,
    /// Vertical coordinate.
    #[sand(default)]
    y: f64,
}

let mut geo = NativeModule::new("geo");
geo.bind::<Point>();
geo.function("mid", |a: Point, b: Point| Point { x: (a.x + b.x) / 2.0, y: (a.y + b.y) / 2.0 });
geo.function("Point.norm", |p: Point| (p.x * p.x + p.y * p.y).sqrt());
```

Scripts construct and access `Point` like any Sand struct (`geo.mid(geo.Point { x: 0.0 }, b).norm()`). Generic type parameters on a `SandEnum` or `SandStruct` map to Sand type parameters; register a generic type using any concrete instantiation (such as `bind::<Pair<(), ()>>()`). Recursive types can reference themselves through `Vec<T>`, maps, or `Box<T>`; conversion aborts with a `"limit"` error if nesting exceeds 256 levels.

Derive `SandObject` on a Rust type to expose opaque reference handles to scripts. Registered functions borrow an object parameter as `&T` and return a new object by value as `T`; scripts compare object handles by identity:

```rust
use sandlang::{NativeModule, NativeObject, SandObject};
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

`NativeModule::async_function` registers an async Rust function; while its future is pending, only the calling Sand task suspends.

To check a sandbox capability inside a native function, register the function with `NativeModule::vm_function` and call `Vm::require`, which returns a `"permission_denied"` error if the policy denies the request. Because the engine re-runs a `vm_function` after a permission prompt or after a garbage collection triggered by a memory or resource check (`Vm::check_memory`, `Vm::reserve_io`, `Vm::resource_slot`), check permissions and limits before performing any side effects. Enable `Config::retry_stress(true)` in tests to force the first memory or resource check in each `vm_function` call to trigger a collection and retry:

```rust
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
use sandlang::vm::Vm;
use sandlang::{Capability, Config, Engine, HostError, NativeModule, Policy, RulePolicy};

let mut door = NativeModule::new("door");
door.vm_function("open", |vm: &mut Vm, name: String| -> Result<String, HostError> {
    vm.require(&Capability::Custom { name: "door".into(), detail: name.clone() })?;
    Ok(format!("opened {name}"))
});
let policy = Policy::rules(RulePolicy::new().allow_custom("door", Some("front")));
let mut engine = Engine::new(Config::new().module(door).policy(policy))?;
engine.run_source("main.sand", "import door\nprint(door.open(\"front\"))\nprint(door.open(\"back\"))").await?;
# Ok(())
# }
```

The script prints:

```text
Ok("opened front")
Err(Error { kind: "permission_denied", message: "permission denied: door back" })
```

`NativeModule::source` attaches Sand source declarations to a native module. Use `native fn` declarations in the Sand source to specify default parameter values or generic type parameters for registered functions. `Engine::new` type-checks registered modules and returns `Error::Compile` if a `native fn` declaration does not match its Rust function signature.

A generic Rust function accepts values of its `N`-th type parameter as `Param<N, B>`, where `B` declares any built-in traits (`bound::Eq`, `bound::Ord`, `bound::Hash`, `bound::Num`, `bound::Encode`, or a tuple such as `(bound::Ord, bound::Hash)`) required by the Rust implementation. The corresponding `native fn` declaration in `NativeModule::source` must declare those bounds (`native fn max<T: Ord>(xs: [T]) -> T?`), directly or through traits that have them as supertraits. If a `native fn` declaration bounds its `N`-th type parameter by `Num`, the Rust function can accept `Num<N>` (providing `zero` and `one`) before the value parameters.

## Interactive sessions

`Engine::session` creates a stateful `Session` that evaluates inputs incrementally, preserving bindings across calls:

```rust
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
# use sandlang::{Config, Engine};
# let config = Config::new();
let mut session = Engine::new(config)?.session();
session.eval("var hits = 0\nfn hit() -> int {\n    hits += 1\n    hits\n}").await?;
assert_eq!(session.eval("hit() + hit()").await?, Some("3".to_string()));
# Ok(())
# }
```

`Session::eval` returns the formatted value of a trailing expression, or `None` for statements and declarations. `Session::type_of(expr)` returns the static type of `expr` without executing it, and `Session::reset()` clears all session state.

## Interrupts

`Engine::interrupt_handle` returns a thread-safe `InterruptHandle` that cancels an in-progress compilation, VM execution, or I/O wait from another thread:

```rust
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
use sandlang::{Config, Engine, Error};
use std::time::Duration;

let mut engine = Engine::new(Config::new())?;
let handle = engine.interrupt_handle();
std::thread::spawn(move || {
    std::thread::sleep(Duration::from_secs(1));
    handle.interrupt();
});
let result = engine.run_source("main.sand", "while true {}").await;
assert!(matches!(result, Err(Error::Interrupted)));
# Ok(())
# }
```

To cancel an individual `Session::eval` call, use `session.interrupt_handle()`.

## Multiple engines

All `Engine` state is local. A single process can embed any number of engines with independent policies, resource limits, and virtual file systems. `Engine` implements `Send`, so engines can move across threads or run concurrently on `LocalSet` tasks.

## WebAssembly

Building `sandlang` with `default-features = false` disables the `os` feature, removing all host OS dependencies and enabling compilation to `wasm32-unknown-unknown` with an in-memory file system.

The `sandlang-wasm` crate in `wasm/` wraps this build in a JavaScript API. Build the ES module bundle with `wasm/build.sh web OUT_DIR`:

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

Run `Engine` inside a Web Worker so that CPU-intensive scripts do not block the browser UI thread. For the full Rust embedding specification, see [§16 of the specification](/spec/#16-embedding-api-rust).
