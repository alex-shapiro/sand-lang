# Sand

> [!NOTE]
> Status: Alpha Software, Handle With Care

Sand is a sandboxed, statically-typed bytecode interpreter written in Rust. Its goal is to be an ergonomic runtime for safely writing and running untrusted code.

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
- Embeddable in a Rust program, or in a web page via webassembly
- Async IO by default
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
sand playground
```

The `sand run` command can run with a security policy. See the [policy file spec §14.7]('docs/SPEC.md) for the full set of policy options ([`docs/SPEC.md` §14.7](docs/SPEC.md)).

```sh
sand run script.sand --policy=app.policy
```

## Embedding

A sand-lang interpreter can be embedded in Rust and import native modules:

```rust
use sandlang::{Config, Engine, Limits, NativeModule, Policy, RulePolicy};

let mut geo = NativeModule::new("geo");
geo.function("dist", |a: f64, b: f64| -> f64 { (a * a + b * b).sqrt() });

let mut engine = Engine::new(Config::new().module(geo));
let mut session = engine.session();
let result = session.eval("import geo\ngeo.dist(3.0, 4.0)").await?;
assert_eq!(result, Some("5.0".to_string()));
```

## In the browser

Sand runs on WebAssembly. You can try it out with the Sand Playground. More info at [`playground/README.md`](playground/README.md).

```sh
playground/build.sh
sand playground
```

## Examples

The `examples/` directory contains a few programs that explore sand-lang's capabilities.

## Acknowledgements

Sand is heavily influenced by Rust, Swift, and Python. Thank you to all of the
coders whose contributions to those languages over the years have made Sand possible.
