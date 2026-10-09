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

- Sandboxed, with resource limits, under policies written as code or text
- Embeddable in a larger process, or in a web page (WebAssembly)
- Async by default
- Types and traits
- Standard library
- Tooling built-in

## Command line

```
sand run script.sand --allow-read=./data --allow-net=api.example.com -- arg1 arg2
sand run script.sand --policy=app.policy   # rules and limits from a policy file
sand run script.sand --prompt          # ask on the terminal for each new access
sand run script.sand --allow-net='*' --allow-private-net   # also reach localhost/LAN addresses
sand check script.sand                 # type check
sand lint script.sand                  # warnings
sand fmt script.sand [--check]         # format
sand test script.sand                  # run `test "..." { }` blocks
sand repl --allow-read=.               # interactive session (run options apply)
sand playground                        # the web playground on localhost (see below)
```

Limits: `--max-memory=256M --max-time=10s --max-cpu=2s --max-instructions=N --max-compile-fuel=N`.
Exit codes: the script's exit code; 1 runtime error; 2 compile error; 3 limit.

A policy file says the same as the options, one rule per line
([`docs/SPEC.md` §14.7](docs/SPEC.md)):

```
# app.policy: paths are relative to this file
allow read ./data
deny read ./data/private      # deny rules win
allow write ./out
allow net api.example.com:443
default prompt                # ask about anything else (default: deny)
limit memory 64M
limit time 10s
```

On a terminal the REPL has a line editor: the arrow keys move the cursor
(Option/Alt-arrows by word, Home/End or Command-arrows to the ends of the
line), Up and Down recall earlier inputs (kept in `~/.sand_history`, or
`$SAND_HISTORY`), Ctrl-R searches them, and an input with open brackets
continues on `...` lines and is edited as a whole. `:help` lists the keys.

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

Without the default `os` feature the engine cannot reach the operating
system at all (no host files, sockets, environment or standard input):
scripts see an in-memory file system the host fills and reads back, which
any engine can also be given with `Config::memory_fs(MemFs)`.

See [`docs/SPEC.md`](docs/SPEC.md) for the language specification and
[`docs/INTERNALS.md`](docs/INTERNALS.md) for the implementation.

## In the browser

The engine also builds for WebAssembly (`wasm/`, JavaScript bindings).
`playground/` is a web page to try Sand: each sandbox has its program (and
extra files) beside its policy, and runs in its own engine in its own Web
Worker, so several run at once.

```
playground/build.sh     # builds the engine for the web (needs wasm-bindgen-cli)
sand playground         # serves it at http://127.0.0.1:8000/
```

See [`playground/README.md`](playground/README.md).

## Examples

`examples/` contains programs that explore sand-lang's capabilities.

## Tests

```
cargo test                        # spec, API, sandbox, examples, property and fuzz tests
cargo test --no-default-features  # the engine without operating-system access
wasm/build.sh nodejs target/wasm-node   # (needs wasm-bindgen-cli; the script says which)
node --test wasm/tests/           # the WebAssembly build and the playground's examples
cargo bench                       # VM benchmarks
cargo +nightly fuzz run compile   # coverage-guided fuzzing (fuzz/)
python3.14 bench/examples/run.py  # the examples against CPython ports (after cargo build --release)
```

CI (`.github/workflows/ci.yml`) runs `cargo fmt --check`, `cargo clippy` and
`cargo test` on Linux, macOS and Windows.
