+++
title = "Embedding"
description = "Embed in a Rust application or web page."
+++

The `sandlang` crate embeds the Sand compiler and VM as a Rust library. Construct a `Config`, create an `Engine`, and run scripts under the configured sandbox policy and resource limits.

## Running a script

`Engine::run_source` compiles and executes a script, returning a future that runs on any Tokio runtime with timers enabled:

```rust
use sandlang::engine::{Config, Engine, Error, Outcome};

async fn run(source: &str) -> Result<Outcome, Error> {
    Engine::new(Config::new()).run_source("main.sand", source).await
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

To run the same script many times, compile it once. `Engine::compile_source` and `Engine::compile_file` compile a script and its imports into a `CompiledProgram`. `Engine::run_program` runs a `CompiledProgram` on any engine. Each run starts with fresh globals.

The compiling engine fixes three parts of a `CompiledProgram`:

- the script and its imported modules
- the policy decisions for those imports
- the host modules

The running engine supplies all other runtime configuration.

```rust
# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
use sandlang::engine::{Config, Engine};

let program = Engine::new(Config::new()).compile_source("main.sand", "print(\"hello\")").await?;
for _ in 0..3 {
    Engine::new(Config::new()).run_program(&program).await?;
}
# Ok(())
# }
```

Failed engine calls return a `sandlang::engine::Error`:

- `Error::Compile(diagnostics)`: compilation failed, or compilation or linting exhausted its fuel.
- `Error::Runtime { message, .. }`: execution failed with an uncaught runtime error.
- `Error::Limit(kind)`: the call exceeded a resource limit.
- `Error::Interrupted`: the host canceled the call through an `InterruptHandle`.
- `Error::Io(message)`: the engine could not read the entry script file.
- `Error::Internal { message, .. }`: an internal invariant failed or a host callback panicked.

`err.render()` formats the error with source snippets and stack traces. `Engine::check_source` type-checks a script without executing it, and `Engine::lint_source` returns lint diagnostics.

## Configuration

`Config::new()` uses `RulePolicy::standard()`, which allows only writes to `stdout` and `stderr`. All other capabilities evaluate to `prompt` and are denied unless you configure a prompter. Use builder methods on `Config` to configure policies, limits, virtual files, and streams:

```rust
# #[tokio::main(flavor = "current_thread")]
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

- **Policy.** `RulePolicy` has `allow_`, `deny_`, and `prompt_` builder methods for each capability kind. Each builder method takes typed arguments. Parse a pattern or `HttpMethod` with `str::parse`. `str::parse` returns a `PatternError` if the text is invalid:
  - `PathPattern` is the path of a file rule. It must be absolute or start with `**`. `PathPattern::literal` matches a host path literally. Use `AbsolutePath::new` in a `const` to validate a path at compile time. Convert an `AbsolutePath` with `PathPattern::from`.
  - `NetPattern` is the host of a `net` rule.
  - `UrlPattern` is the URL of an `http` rule.
  - `ListenPattern` is the address of a `listen` rule.
  - `NamePattern` is the variable name of an `env` rule. `NamePattern::exact` matches a literal variable name.
  - `Transports` selects `Tls`, `Plaintext`, or `Both` for a `net` or `http` rule.
  - `Methods` selects the HTTP methods for an `http` rule. `Methods::all()` matches every method. `Methods::only(first, rest)` matches specific `HttpMethod` values.
  - `CustomMatch` selects `Any` or `Exact(text)` for the name and detail of a `custom` rule.
- **Default rules.** `RulePolicy::default()` is empty. `RulePolicy::standard()` allows `stdout` and `stderr`. `RulePolicy::allow_all()` allows all capabilities for trusted scripts. `user_rules.with_defaults(host_rules)` adds fallback rules for requests that do not match `user_rules`. `Config::custom_policy` replaces rule evaluation with a host-defined `CustomPolicy`.
- **Limits.** `Limits` configures every resource bound described in [Sandbox and policies](/guide/sandbox/).
- **Virtual file system.** `Config::memory_fs` replaces host disk access with an in-memory `MemFs`. Clones of a `MemFs` share the same underlying storage so the host can populate input files before a run and read output files afterward.
- **Streams.** `Config::stdout` and `Config::stderr` redirect script output to a `MemorySink` or custom sink. `Config::stdin_bytes` supplies fixed input bytes. `Config::stdin_feed` and `Config::stdin_io_feed` return a `StdinFeed` that the host uses to send input while the script runs.
- **Logging.** `Config::log_sink` routes `log` records to a host `LogSink` as a `LogRecord` containing the level, message, fields, and caller location. `LogSink::enabled` filters levels before record construction, and `Config::log_level` sets the initial script log level. Emitted records count against the output limit.
- **Environment.** `Config::args` and `Config::env` set the command-line arguments and environment map exposed to the script. If `Config::env` is not set, `env.get` reads the host process environment subject to the policy.

To load a `.policy` file from disk, parse it with `sandlang::policy::file::PolicyFile`:

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

The second argument is the base directory for `./` paths. The third argument is the optional home directory for `~/` paths. Appending `RulePolicy::standard()` via `with_defaults` allows `stdout` and `stderr` unless overridden by a rule in the policy file, such as `prompt stdout` or `deny stdout`.

`apply_limits` applies the `limit` directives in the file to the limits that you pass. Limits that the file does not set keep their original values.

Inspect `PolicyFile::rule_directives` and `PolicyFile::limit_directives` to reject capabilities or limit values that your application does not support. Each directive includes its 1-based line and column for error reporting.

To handle `prompt` decisions, pass an async callback to `Config::prompter` that accepts a `Capability` and returns a `PromptAnswer`. If a prompter is not configured, the engine denies `prompt` decisions.

## Native modules

`NativeModule` exposes Rust functions to Sand scripts as an importable module. `HostModules::new` compiles native modules. `Config::host_modules` registers them on a `Config`:

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

Clones of a `HostModules` share the compiled modules. Create a `HostModules` once and pass clones to each `Config`.

```sand ignore
import geo

print(geo.dist(3.0, 4.0))
print(geo.country("XX"))
```

Running the script above prints `5.0` and `Err(Error(message: "unknown country XX", kind: "not_found"))`.

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
| `jiff::Timestamp`                                            | `time.Instant`               |
| `jiff::civil::DateTime`                                      | `time.PlainDateTime`         |
| `jiff::civil::Date`                                          | `time.PlainDate`             |
| `jiff::civil::Time`                                          | `time.PlainTime`             |
| `sandlang::stdlib::json::Json`                               | `json.Json`                  |
| tuples of up to 8 elements                                   | tuples                       |
| Rust enum with `#[derive(SandEnum)]`                         | matching Sand `enum`         |
| Rust struct with `#[derive(SandStruct)]`                     | matching Sand `struct`       |
| `&T` parameter or `T` return with `#[derive(SandObject)]`    | opaque type `T`              |
| `ValueType` `T` with `#[derive(SandValue)]`                  | opaque value type `T`        |

Add `indexmap` 2 or `jiff` 0.2 to `Cargo.toml` to use `IndexMap` or the `jiff` types directly. `sandlang::stdlib::time` defines the remaining `time` types.

The engine automatically exports registered functions that are not declared in `NativeModule::source` as `pub`, with default parameter names `arg1`, `arg2`, ... unless overridden by `params`. Registering a function as `"Type.name"` defines a method on `Type`; the method takes `self` if its first parameter has type `Type` (such as `Counter.add` below). Generic functions must be declared with `native fn` in `NativeModule::source`.

Returning `Err(HostError::new(kind, message))` produces a Sand `Err(Error)` with the given `kind` and `message`. The engine also converts `std::io::Error` into an `Error` with its standard kind, and `String` or `&str` errors into an `Error` with kind `"error"`. A function that returns `Result<T, sandlang::vm::Fail>` has the Sand return type `T`. Returning `Err(fail)` aborts the run with a runtime error (`Fail::runtime_error`), a resource limit error, or an exit code (`Fail::exit`).

Each derive macro (`SandEnum`, `SandStruct`, `SandObject`, `SandValue`) binds a Rust type to a Sand type in the module specified by `#[sand(module = "...")]`. Register each bound type with `NativeModule::bind`. Do not redeclare a bound type in `NativeModule::source`; use `extend` to add Sand methods to it. Optional type attributes override the Sand name (`#[sand(name = "Kind")]`), make the type private to its module (`#[sand(private)]`), or specify a renamed `sandlang` dependency (`#[sand(crate = my_sandlang)]`).

Derive `SandEnum` on a Rust enum to pass enum values between Rust and Sand:

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

The engine generates the `pub enum Shape` declaration in `geo` from the Rust variants and doc comments, so scripts can call `geo.area(geo.Shape.Rect(w: 2.0, h: 3.0))`.

Derive `SandStruct` on a Rust struct with named fields to copy struct values between Rust and Sand. Fields are `pub` and immutable by default; annotate a field with `#[sand(private)]` to make it private, `#[sand(var)]` to make it mutable, `#[sand(name = "kind")]` to override its Sand name, or `#[sand(default)]` (`#[sand(default = 1.0)]`) to give it a default value:

```rust
use sandlang::SandStruct;
use sandlang::native::NativeModule;

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

`NativeModule::function` accepts three kinds of Rust function, selected by the function signature:

- **Synchronous.** Each parameter can borrow `&str`, `&[u8]`, or `&T` from the VM heap.
- **Synchronous with `&mut Vm` first.** A synchronous function that accesses the VM. Its other parameters are owned types or VM handles.
- **Asynchronous.** A function that returns a future and suspends only the calling Sand task. Its parameters, future, and future output are owned `Send` types.

The compiler rejects a VM handle such as `sandlang::native::ty::VmVal` in a future, because garbage collection during the wait can free the heap object. The compiler does not check a raw `VmValue`. Do not hold a raw `VmValue` across a wait.

To return a recoverable error from a future, use `Result<T, HostError<sandlang::vm::Fail>>`. The future cannot make a scheduler request such as a permission check. If an operation needs a permission, take `&mut Vm`, call `Vm::require` before the wait, and return a `sandlang::native::func::Later`.

To check a sandbox capability inside a native function, take `&mut Vm` as the first parameter and call `Vm::require`. `Vm::require` returns a `"permission_denied"` error if the policy denies the request. A function that takes `&mut Vm` must check permissions and limits before any side effect. The engine runs the function again after a permission prompt or a garbage collection during a memory or resource check. Enable `Config::retry_stress(true)` in tests to collect garbage and retry on the first memory or resource check in each call:

```rust
# #[tokio::main(flavor = "current_thread")]
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

The script prints:

```text
Ok("opened front")
Err(Error(message: "permission denied: door back", kind: "permission_denied"))
```

`NativeModule::source` attaches Sand source declarations to a native module. Use `native fn` declarations in the Sand source to specify default parameter values or generic type parameters for registered functions. `HostModules::new` type-checks each module against its registered Rust functions. It returns `Error::Compile` if a `native fn` declaration does not match its Rust signature.

A generic Rust function accepts values of its `N`-th type parameter as `Param<N, B>`, where `B` declares any built-in traits (`traits::Eq`, `traits::Ord`, `traits::Hash`, `traits::Num`, `traits::Encode`, or a tuple such as `(traits::Ord, traits::Hash)`) required by the Rust implementation. The corresponding `native fn` declaration in `NativeModule::source` must require those traits (`native fn max<T: Ord>(xs: [T]) -> T?`), directly or through traits that have them as supertraits. If a `native fn` declaration requires `Num` of its `N`-th type parameter, the Rust function can accept `Num<N>` (providing `zero` and `one`) before the value parameters.

## Values outside the VM

`sandlang::value::Value` holds a Sand value in a Rust form. A `Value` does not reference the VM, so you can keep it after the run ends. A struct or an enum variant in a `Value` has its type name with its module, such as `shop.Order`. The `time` types have their own variants, such as `Value::Instant` and `Value::PlainDate`. Each other native value type is a `Value::Native` with its type name and text.

`Vm::to_value` copies any script value out of the VM. A value whose type does not implement `Encode`, such as a function or a native object, becomes `Value::Opaque`. The copy charges fuel and counts against the memory limit, and it fails with a `"limit"` error beyond 512 levels of nesting:

```rust
use sandlang::native::NativeModule;
use sandlang::native::ty::Param;
use sandlang::value::Value;
use sandlang::vm::{Fail, Vm};

let mut audit = NativeModule::new("audit");
audit.source("pub native fn record<T>(event: T)");
audit.function("record", |vm: &mut Vm, event: Param<0>| -> Result<(), Fail> {
    if let Value::Struct(event) = vm.to_value(event.raw())? {
        println!("{} with {} fields", event.type_name, event.fields.len());
    }
    Ok(())
});
```

The trait `sandlang::value::Encode` gives the `Value` form of a Rust value without a VM. Primitives, strings, collections, `Option`, tuples, and native value types implement it. `SandStruct`, `SandEnum`, and `SandValue` derive it. The `Value` of a derived type is equal to the copy that `Vm::to_value` makes of the same Sand value:

```rust
use sandlang::SandStruct;
use sandlang::value::{Encode, Value};

/// A point in the plane.
#[derive(SandStruct)]
#[sand(module = "geo")]
struct Point {
    x: f64,
    y: f64,
}

let Value::Struct(point) = Point { x: 1.0, y: 2.0 }.encode() else { unreachable!() };
assert_eq!(&*point.type_name, "geo.Point");
assert_eq!(point.fields[0], ("x".into(), Value::Float(1.0)));
```

`Value::native::<T>()` returns a value as the native value type `T`, such as `value.native::<jiff::civil::Date>()`, or `None` if the value is not a `T`.

A `#[sand(default)]` field default is the `Encode` form of its Rust value. The default must be a Sand constant, so a native value type such as `jiff::Timestamp` cannot be a field default. A derived type with a field that has no `Value` form, such as a `VmVal` handle, takes `#[sand(no_encode)]`.

## Interactive sessions

`Session::new` creates a stateful `Session` that evaluates inputs incrementally, preserving bindings across calls:

```rust
# #[tokio::main(flavor = "current_thread")]
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

`Session::eval` returns the formatted value of a trailing expression, or `None` for statements and declarations. `Session::type_of(expr)` returns the static type of `expr` without executing it, and `Session::reset()` clears all session state.

## Interrupts

Use an `InterruptHandle` to stop an active compilation, VM execution, or I/O wait from another thread. Pass the handle to `Config::interrupt` before creating the engine or session:

```rust
# #[tokio::main(flavor = "current_thread")]
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

In a `Session`, `InterruptHandle::interrupt` cancels an active `eval`, `run_tests`, or `type_of` call. In `run_tasks_until`, an interrupt cancels the background tasks and passes `Error::Interrupted` to `on_error`. The call then continues to wait for `until`. You can pass clones of a handle to multiple configurations to stop all of their engines and sessions together. An engine or session ignores calls to `interrupt()` that occur before a call starts.

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
