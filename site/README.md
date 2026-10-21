# sandlang.com

The website: a home page, the guide, the library reference, the
specification and the playground. A Rust program in this crate builds it
into static files.

```sh
cargo site serve            # build, then serve at http://127.0.0.1:8000/
cargo site build            # build into target/site; exit code 1 if a check fails
```

`build` makes the playground's engine with `wasm/build.sh` (it needs the
`wasm32-unknown-unknown` target and `wasm-bindgen-cli`; the script says how
to install them). `--engine=dev` builds it faster, and `--engine=none`
keeps the engine of the previous build, and fails if there is none. `serve` uses `--engine=dev` unless
told otherwise, and serves the site even if a check fails.

## Why a custom generator

The generator uses the Sand compiler as a library, so the site cannot
drift from the language:

- It checks the Sand examples of the guide and the home page, runs them,
  and shows what they printed.
- It highlights Sand with the compiler's own lexer.
- It generates the library reference from the declarations and doc
  comments of the standard library (`Engine::docs`).
- It checks every internal link and anchor.

Its only other dependencies are `pulldown-cmark` and Tokio. It needs no
JavaScript runtime.

## Layout

| Path                   | Contents                                                     |
| ---------------------- | ------------------------------------------------------------ |
| `content/index.md`     | the home page                                                |
| `content/guide/*.md`   | the guide's chapters, in file name order (`NN-slug.md`)      |
| `static/`              | the stylesheet, script, logo and icons, copied as they are   |
| `playground/`          | the playground, served at `/play/` ([README](playground/README.md)) |
| `src/`                 | the generator                                                |
| `tests/`               | the playground's examples, run in Node.js with the WebAssembly engine |
| `icons.sh`             | renders the PNG icons from the logo                          |

A content file starts with front matter:

```text
+++
title = "Errors"
description = "One sentence for search results and the page's description."
+++
```

## Code blocks

The info string of a code block gives its language: `sand`, `policy`,
`rust`, `js`, `sh` or `text`. A `policy` block must parse as a policy file. A
`sand` block must be a whole program, formatted as `sand fmt` formats it,
and must run without an error; the page shows its output below it. A word
after `sand` changes the check:

| Info string   | Check                                              |
| ------------- | -------------------------------------------------- |
| `sand`        | runs; the page shows the output                    |
| `sand no_run` | type checks                                        |
| `sand error`  | fails to compile; the page shows the diagnostic    |
| `sand fails`  | fails at run time; the page shows the error        |
| `sand ignore` | no check (for fragments)                           |

Lines that start with `# ` are part of the program but are not shown.
Examples run with an empty memory file system, a fixed random seed and the
policy `POLICY` of `src/examples.rs`: they may read and write files, and do
nothing else. "Open in playground" gives the playground the same policy. In doc comments and in the specification (`docs/SPEC.md`, served at `/spec/`), Sand blocks are fragments: the generator highlights them, but does not check them.

A `rust` block must compile. `cargo test -p sandlang-site` compiles each
one as the body of an async function that returns a `Result`, or as it is
if it defines `fn main` (`build.rs`). Lines that start with `# ` are
compiled but not shown. `rust ignore` skips the check.

## Logo and icons

`static/logo.svg` is the source of the logo. `static/favicon.svg` is a copy
with fewer, larger grains of sand, so that they show at the size of a
browser tab; change it with the logo. `icons.sh` renders
`static/apple-touch-icon.png` and the link preview image `static/og.png`
from the logo with headless Chrome.

## Deploying

Copy `target/site` to any static host. The host must serve `index.html`
for a directory, `404.html` for a missing page, and `.wasm` files as
`application/wasm`.
