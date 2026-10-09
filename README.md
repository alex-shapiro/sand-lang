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
* **Traits**: user-defined interfaces with default and static methods,
  supertraits, type parameters, and generic or conditional impls
  (`impl<T: Describe> Describe for [T]`), usable as bounds
  (`fn show<T: Describe>(x: T)`). Calls on concrete types are statically
  dispatched; generic code receives dictionaries.
* **Lazy iterators**: `xs.iter().map(f).filter(g).take(10)`, endless sources
  (`Iter.count_from(1)`), and `for` loops over any type implementing the
  `Iterator<T>` trait.
* **Elegant syntax**: newline-terminated statements, expression blocks,
  string interpolation with format specs, comprehensions, lambdas (`x => x * 2`),
  pipes, named and default arguments, duration literals (`250ms`), labeled
  `break`/`continue` (`break @outer`), and `defer` for cleanup on every exit
  path (`defer conn.close()`).
* **Async by default**: any function may suspend on I/O without `async`
  colouring; `spawn`, `task.all`, `task.race`, `task.timeout`, channels.
* **Sandboxed**: every file, network, environment, stdin/stdout and import
  access is checked against a host policy, which may prompt the user (all
  execution pauses during the prompt). Network destinations are checked after
  name resolution (private ranges can be blocked), and file operations cannot
  be redirected by symlinks swapped in after the check. No FFI, subprocesses,
  IPC or mmap.
* **Resource limits**: memory, instructions, CPU time, wall time, call depth,
  tasks, open resources, output size, HTTP body size, compile work; plus an
  interrupt handle. Work inside library calls is metered too, so limits and
  interrupts apply while a huge sort or `json.parse` is running.
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
sand run script.sand --allow-net='*' --allow-private-net   # also reach localhost/LAN addresses
sand check script.sand                 # type check
sand lint script.sand                  # warnings
sand fmt script.sand [--check]         # format
sand test script.sand                  # run `test "..." { }` blocks
sand repl --allow-read=.               # interactive session (run options apply)
```

The REPL runs each input once in a persistent VM: definitions, globals and
tasks carry over to later inputs, which may redefine names; spawned tasks keep
running while the REPL waits for input. `:type EXPR`, `:test`, `:reset` and
`:help` are available; Ctrl-C interrupts a running input (or, at an empty
prompt, cancels background tasks).

Limits: `--max-memory=256M --max-time=10s --max-cpu=2s --max-instructions=N --max-compile-fuel=N`.
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

// Notebooks and tools: run inputs one at a time against persistent state.
let mut session = engine.session();
session.eval("import geo\nlet d = geo.dist(3.0, 4.0)").await?;
assert_eq!(session.eval("d * 2.0").await?, Some("10.0".to_string()));
```

See [`docs/SPEC.md`](docs/SPEC.md) for the language specification and
[`docs/INTERNALS.md`](docs/INTERNALS.md) for the implementation.

## Examples

`examples/` contains complete programs: a Jinja-style template engine, an HTTP
page fetcher, a TCP chat server, Conway's Game of Life, a calculator language
interpreter, a text adventure, a persisted inverted index with TF-IDF search,
an expense report built from generic traits, and an S3 client with AWS
Signature V4.

## Tests

```
cargo test                      # spec, API, sandbox, examples, property and fuzz tests
cargo bench                     # VM benchmarks
cargo +nightly fuzz run compile # coverage-guided fuzzing (fuzz/)
```

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy` and
`cargo test` on Linux, macOS and Windows.

## Dependencies

`tokio` (async I/O), `regex-automata` (the engine of the `regex` crate, used
directly so each compiled regex's memory can be measured),
`unicode-normalization` (matching path deny rules the way case- and
normalisation-insensitive file systems compare names), `sha2` (SHA-256 with
the CPU's SHA instructions, in Rust), `rustix` (on Unix) and
`fs_at` and `same-file` (on Windows) for file access that never follows
symlinks, and — behind the default `tls`
feature — `rustls`,
`tokio-rustls` and `webpki-roots` for HTTPS. Note that rustls's `ring`
crypto provider contains C and assembly; build with
`--no-default-features` for a pure-Rust build without HTTPS.
