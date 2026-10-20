+++
title = "Embedding"
description = "Run Sand scripts from a Rust program or a web page, with your own policy, limits, files and native modules."
+++

The `sandlang` crate is the Sand engine as a Rust library. Your program creates an `Engine`, gives it a `Config`, and runs scripts. The script can do only what the configuration allows.

## Run a script

`Engine::run_source` compiles and runs a script. It returns a future: run it on a Tokio runtime with timers enabled.

```rust
use sandlang::{Config, Engine};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut engine = Engine::new(Config::new());
    match engine.run_source("main.sand", r#"print("hello from Sand")"#).await {
        Ok(outcome) => println!("exit code {}", outcome.exit_code),
        Err(err) => eprint!("{}", err.render()),
    }
}
```

The script runs inside the future, on the thread that polls it. The first argument is the file name that diagnostics and stack traces show. `run_file(path)` runs a file instead.

`Error` tells you why a run failed:

| Variant                           | Cause                                     |
| --------------------------------- | ----------------------------------------- |
| `Error::Compile(diagnostics)`     | the script has syntax or type errors      |
| `Error::Runtime { message, .. }`  | the script failed with an error           |
| `Error::Limit(kind)`              | the script went over a resource limit     |
| `Error::Interrupted`              | the host interrupted the run              |
| `Error::Internal { message, .. }` | a bug in the engine or in a native module |

`err.render()` formats the error as `sand run` shows it. `check_source(name, source)` type checks a script without running it, and `lint_source` returns its lint warnings.

## Configuration

`Config::new()` denies all access except standard output and standard error. Add a policy, limits and other settings with builder methods:

```rust
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

let mut engine = Engine::new(config);
engine.run_source("main.sand", script).await?;
println!("{}", out.contents());
let report = files.read("/out/report.txt")?;
```

- **Policy.** `RulePolicy` has a method for each rule of a policy file: `allow_read`, `allow_write`, `allow_net`, `allow_http`, `allow_env`, `deny_read`, `deny_net`, `fallback` and more. `Policy::allow_all()` allows everything; use it only for trusted scripts.
- **Limits.** `Limits` has a field for each limit of [Sandbox and policies](/guide/sandbox/). Start from `Limits::default()`.
- **Files.** `memory_fs` gives the scripts an in-memory file system instead of the disk. Clones of a `MemFs` share their files, so the host can write input files and read the output files.
- **Streams.** `stdout` and `stderr` capture the output of the script. `stdin_bytes` gives it a fixed standard input, which the policy must allow with `stdin`.
- **Environment.** `args` and `env` set what `env.args()` and `env.get` return. Without `env`, scripts read the environment of your process, if the policy allows it.

To use a policy file, parse it with `sandlang::policy::PolicyFile`:

```rust
# use sandlang::Config;
use sandlang::policy::PolicyFile;
use std::path::Path;

let text = std::fs::read_to_string("app.policy")?;
let file = PolicyFile::parse(&text, Path::new("."), None).expect("a valid policy file");
let config = Config::new().policy(file.policy()).limits(file.limits.clone());
```

The second argument is the directory for `./` paths, and the third is the directory for `~/` paths.

To answer `prompt` decisions, give `Config::prompter` a function that takes a `Capability` and returns a future of a `PromptAnswer`. Without a prompter, Sand denies each request that the policy would prompt for.

## Native modules

A `NativeModule` makes Rust functions available to scripts. Scripts import it like a standard module:

```rust
use sandlang::{Config, HostError, NativeModule};

let mut geo = NativeModule::new("geo");
geo.function("dist", |x: f64, y: f64| -> f64 { (x * x + y * y).sqrt() })
    .params(&["x", "y"]);
geo.doc("dist", "The distance of (x, y) from the origin.");
geo.function("country", |code: &str| -> Result<String, HostError> {
    match code {
        "NL" => Ok("Netherlands".to_string()),
        _ => Err(HostError::new("not_found", format!("no country {code}"))),
    }
});
let config = Config::new().module(geo);
```

```sand ignore
import geo

print(geo.dist(3.0, 4.0))
print(geo.country("XX"))
```

The script prints `5.0` and `Err(Error { kind: "not_found", message: "no country XX" })`.

Sand derives the signature of each function from its Rust types. Without `.params`, the parameters have the names `arg1`, `arg2` and so on. These Rust types convert to Sand types:

| Rust                                        | Sand                         |
| ------------------------------------------- | ---------------------------- |
| `i64`, `f64`, `bool`, `()`                  | `int`, `float`, `bool`, `()` |
| `String`, `&str`                            | `str`                        |
| `Vec<u8>`, `&[u8]`                          | `bytes`                      |
| `Vec<T>`                                    | `[T]`                        |
| `Option<T>`                                 | `T?`                         |
| `Result<T, E>`                              | `Result<T, Error>`           |
| `HashMap<String, T>`, `BTreeMap<String, T>` | `[str: T]`                   |
| tuples of up to 4 elements                  | tuples                       |

A `HostError` keeps its kind. Any other error type becomes an `Error` of kind `error`, with the `Display` text of the error as its message.

`async_function` registers a function that returns a future. While the future runs, only the calling task waits; other tasks of the script continue.

`guarded_function` checks a capability before it calls the function. The guard computes the capability from the arguments. In Sand, the function returns a `Result`, which is an `Err` of kind `permission_denied` if the policy denies the capability:

```rust
use sandlang::{Capability, NativeModule, Policy, RulePolicy};

let mut door = NativeModule::new("door");
door.guarded_function(
    "open",
    |name: &String| Capability::Custom { name: "door".into(), detail: name.clone() },
    |name: String| -> String { format!("opened {name}") },
);
let policy = Policy::rules(RulePolicy::new().allow_custom("door", Some("front")));
```

`NativeModule::source` adds Sand declarations to a module, such as types and helper functions written in Sand.

## Sessions

A `Session` runs inputs one at a time, like `sand repl`. Each input can use the names of the inputs before it:

```rust
# use sandlang::{Config, Engine};
# let config = Config::new();
let mut session = Engine::new(config).session();
session.eval("var hits = 0\nfn hit() -> int {\n    hits += 1\n    hits\n}").await?;
assert_eq!(session.eval("hit() + hit()").await?, Some("3".to_string()));
```

`eval` returns the debug form of the value of the input, or `None` if it has no value. `type_of(expr)` gives the type of an expression without running it, and `reset()` forgets all inputs.

## Interrupts

An `InterruptHandle` stops a run from another thread. It can stop the engine while the engine compiles, runs code, or waits for I/O:

```rust
use sandlang::{Config, Engine, Error};
use std::time::Duration;

let mut engine = Engine::new(Config::new());
let handle = engine.interrupt_handle();
std::thread::spawn(move || {
    std::thread::sleep(Duration::from_secs(1));
    handle.interrupt();
});
let result = engine.run_source("main.sand", "while true {}").await;
assert!(matches!(result, Err(Error::Interrupted)));
```

For a fixed time limit, set `max_wall_time` in `Limits` instead. The handle of an engine does not stop its sessions. To stop a session input, use `session.interrupt_handle()`.

## Multiple engines

Engines share no mutable state. One process can contain many engines, each with its own policy, limits and files. An `Engine` is `Send`, so you can run engines on different threads, or run many engines on one thread at the same time. A script in one engine cannot see or change another engine.

## On the web

Without the default `os` feature (`default-features = false`), the engine cannot reach the operating system, whatever the policy says. The scripts get an in-memory file system, and the crate builds for `wasm32-unknown-unknown`.

The `sandlang-wasm` crate in `wasm/` is a JavaScript API for this build. `wasm/build.sh web OUT_DIR` builds it as an ES module:

```js
import init, { Engine } from "./pkg/sandlang_wasm.js";

await init();
const engine = new Engine(
  { policy: "allow /data", files: { "/data/in.txt": "hello" } },
  { onOutput: (stream, text) => console.log(stream, text) },
);
const result = await engine.run('import fs\nprint(fs.read_text("/data/in.txt")?)');
console.log(result.ok, result.exitCode);
```

The options take a policy file as text, the files, the environment, the arguments and standard input. The `onPrompt` hook answers requests that the policy does not decide. A run uses the thread of the page while it computes, so run engines in Web Workers to keep the page responsive. The [playground](/play/) uses this API.

[§16 of the specification](https://github.com/alex-shapiro/sand-lang/blob/main/docs/SPEC.md#16-embedding-api-rust) describes the full API.
