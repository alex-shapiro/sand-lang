# Sand

Sand is a sandboxed bytecode interpreter written in Rust. Its primary goal
is to be the most ergonomic runtime for safely writing and running untrusted code.

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

- Sandboxed, with policy-defined limits on the runtime and on resource access
- Embeddable in a larger process, or in a web page via webassembly
- Async by default
- Support for types and traits
- Standard library
- Formatter, linter, and tester built in

## Command line

Install the `sand` command line tool:

```
cargo install --path cli
```

Common commands:

```sh
sand repl
sand run script.sand
sand check script.sand
sand lint script.sand
sand fmt script.sand
sand test script.sand
sand playground # the web playground on localhost
```

The `sand run` command lets you apply a policy with either a policy file or with args.

```sh
sand run script.sand --policy=app.policy
sand run script.sand --allow-read=./data --allow-net=api.example.com -- arg1 arg2
sand run script.sand --allow-net='*' --allow-private-net
sand run script.sand --prompt # prompt the user whenever they Sand tries to access a new resource
```

See the [policy file spec §14.7]('docs/SPEC.md) for the full set of policy options ([`docs/SPEC.md` §14.7](docs/SPEC.md)).

## Embedding

Multiple sand-lang interpreters can be embedded in a larger Rust program.
By default, an interpreter imports modules that permit policy-approved access
to OS resources like the filesystem and network.

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
let mut session = engine.session();
session.eval("import geo\nlet d = geo.dist(3.0, 4.0)").await?;
assert_eq!(session.eval("d * 2.0").await?, Some("10.0".to_string()));
```

## In the browser

Sand runs on WebAssembly. You can try it out with the Sand Playground.
More info at [`playground/README.md`](playground/README.md).

```sh
playground/build.sh # builds the engine for the web (needs wasm-bindgen-cli)
sand playground     # serves it at http://127.0.0.1:8000/
```

## Examples

The `examples/` directory contains a few programs that explore sand-lang's capabilities.

## Acknowledgements

Sand is heavily influenced by Rust, Swift, and Python. Thank you to all of the
coders whose contributions to those languages over the years have made Sand possible.
