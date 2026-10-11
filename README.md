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
        Ok(page) => print("\(page.url): \(page.title ?? "(no title)")")
        Err(e) => eprint("failed: \(e)")
    }
}
```

## Highlights

- Sandboxed, with resource limits, under policies written as code or text
- Resources: `use f = fs.open(path)?` closes `f` when its block ends, and
  the linter reports resources that are never closed or that escape
- Embeddable in a larger process, or in a web page (WebAssembly)
- Async by default
- Types and traits
- Standard library
- Tooling built-in

## Command line

The `sand` command is the `sand-cli` package (`cli/`): `cargo install --path cli`.

```
sand run script.sand --allow-read=./data --allow-net=api.example.com -- arg1 arg2
sand run script.sand --policy=app.policy   # rules and limits from a policy file
sand run script.sand --prompt          # ask on the terminal for each new access
sand run script.sand --allow-net='*' --allow-private-net   # also reach localhost/LAN addresses
sand check script.sand                 # type check
sand lint script.sand                  # warnings
sand fmt script.sand [--check]         # format (`sand fmt -`: standard input to output)
sand test script.sand                  # run `test "..." { }` blocks
sand repl --allow-read=.               # interactive session (run options apply)
sand playground                        # the web playground on localhost (see below)
```

Limits: `--max-memory=256M --max-time=10s --max-cpu=2s --max-instructions=N --max-compile-fuel=N`.
Exit codes: the script's exit code; 1 runtime error; 2 compile error (also
for `sand check`); 3 limit; 130 interrupted.

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

On a terminal the REPL has a line editor with history (`~/.sand_history`, or
`$SAND_HISTORY`); `:help` lists the keys. `help()` gives an overview of the
language's functions, types and modules, `help(x)` documents a module, type,
function or any value, and `members(x)` lists what may follow `x.`.

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

The default feature `os` gives access to the operating system; without it,
scripts see an in-memory file system the host fills and reads back
(`Config::memory_fs`). `tls` adds TLS for `https` and `net.connect_tls`.

See [`docs/SPEC.md`](docs/SPEC.md) for the language specification and
[`docs/INTERNALS.md`](docs/INTERNALS.md) for the implementation.

## In the browser

The engine also builds for WebAssembly (`wasm/`, JavaScript bindings).
`playground/` is a web page to try Sand, each sandbox in its own engine and
Web Worker.

```
playground/build.sh     # builds the engine for the web (needs wasm-bindgen-cli)
sand playground         # serves it at http://127.0.0.1:8000/
```

See [`playground/README.md`](playground/README.md).

## Examples

`examples/` contains programs that explore sand-lang's capabilities.

## Tests

```
cargo test                                     # library and cli, with TLS
cargo test -p sandlang --no-default-features   # engine without operating-system access
wasm/build.sh nodejs target/wasm-node          # needs wasm-bindgen-cli
node --test wasm/tests/                        # wasm build and the playground examples
cargo bench                                    # VM and front-end benchmarks
cargo +nightly fuzz run compile                # coverage-guided fuzzing
python3.14 bench/run.py                        # vs Lua 5.5 and CPython 3.14 (bench/README.md)
```

CI runs `cargo fmt --check`, `cargo clippy`, and `cargo test` on macOS, Windows, Linux, and WebAssembly.
