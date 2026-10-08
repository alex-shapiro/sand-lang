# Sand

Sand is a small, statically typed, sandboxed scripting language with a fast
register-based bytecode VM, written in 100% safe Rust (`#![forbid(unsafe_code)]`).

```
import http
import regex

struct Page {
    url: str
    title: str?
}

fn fetch(url: str) -> Result<Page> {
    let resp = http.get(url, timeout: 10s)?
    let title = regex.new(r"<title>(.*?)</title>")?.captures(resp.text()?).and_then(c => c.text(1))
    Ok(Page { url, title })
}

let pages = ["https://example.com", "https://example.org"].map(u => spawn fetch(u))
for p in task.all(pages) {
    match p {
        Ok(page) => print("{page.url}: {page.title ?? "(no title)"}")
        Err(e) => eprint("failed: {e}")
    }
}
```

## Highlights

* **Typed and inferred**: annotated signatures, inferred bodies, generics with
  bounds (`Eq`, `Ord`, `Hash`, `Num`), structs, enums with exhaustive pattern
  matching, `Option`/`Result` with `?` and `??`, no null, checked integers.
* **Elegant syntax**: newline-terminated statements, expression blocks,
  string interpolation with format specs, comprehensions, lambdas (`x => x * 2`),
  pipes, named and default arguments, duration literals (`250ms`).
* **Async by default**: any function may suspend on I/O without `async`
  colouring; `spawn`, `task.all`, `task.race`, `task.timeout`, channels.
* **Sandboxed**: every file, network, environment, stdin/stdout and import
  access is checked against a host policy, which may prompt the user (all
  execution pauses during the prompt). No FFI, subprocesses, IPC or mmap.
* **Resource limits**: memory, instructions, CPU time, wall time, call depth,
  tasks, open resources, output size, HTTP body size; plus an interrupt handle.
* **Embeddable**: `Engine` is `Send`; any number of engines can run
  concurrently in one process. Host modules expose Rust closures with types
  derived automatically.
* **Tooling built in**: type checker, linter, formatter, test runner, REPL.
* **Standard library**: base64, crypto, env, fs, help, http (client and
  server), io, json (incl. typed `decode<T>`), log, math, net, random, regex,
  statistics, task, test, time, uri, uuid.

## Command line

```
sand run script.sand --allow-read=./data --allow-net=api.example.com -- arg1 arg2
sand run script.sand --prompt          # ask on the terminal for each new access
sand check script.sand                 # type check
sand lint script.sand                  # warnings
sand fmt script.sand [--check]         # format
sand test script.sand                  # run `test "..." { }` blocks
sand repl
```

Limits: `--max-memory=256M --max-time=10s --max-cpu=2s --max-instructions=N`.
Exit codes: the script's exit code; 1 runtime error; 2 compile error; 3 limit.

## Embedding

```rust
use sandlang::{Config, Engine, Limits, NativeModule, Policy, RulePolicy};

let mut geo = NativeModule::new("geo");
geo.function("dist", |a: f64, b: f64| -> f64 { (a * a + b * b).sqrt() });

let config = Config::new()
    .policy(Policy::rules(RulePolicy::new().allow_read("/srv/data")))
    .limits(Limits { max_memory: Some(64 << 20), ..Limits::default() })
    .module(geo);
let mut engine = Engine::new(config);
let outcome = engine.run_source("main.sand", "import geo\nprint(geo.dist(3.0, 4.0))").await?;
```

See [`docs/SPEC.md`](docs/SPEC.md) for the language specification and
[`docs/INTERNALS.md`](docs/INTERNALS.md) for the implementation.

## Examples

`examples/` contains complete programs: a Jinja-style template engine, an HTTP
page fetcher, a TCP chat server, Conway's Game of Life, a calculator language
interpreter, a text adventure, a persisted inverted index with TF-IDF search,
and an S3 client with AWS Signature V4.

## Tests

```
cargo test                      # spec, API, sandbox, examples, property and fuzz tests
cargo bench                     # VM benchmarks
cargo +nightly fuzz run compile # coverage-guided fuzzing (fuzz/)
```

## Dependencies

`tokio` (async I/O), `regex`, and — behind the default `tls` feature — `rustls`,
`tokio-rustls` and `webpki-roots` for HTTPS. Note that rustls's `ring`
crypto provider contains C and assembly; build with
`--no-default-features` for a pure-Rust build without HTTPS.
