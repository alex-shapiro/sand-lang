# sandlang.com

This crate builds the static files for `sandlang.com`, including the home page, guide, library reference, specification, and browser playground.

```sh
cargo site serve            # build and serve at http://127.0.0.1:8000/
cargo site build            # build into target/site; exit with 1 if a check fails
```

`cargo site build` compiles the WebAssembly playground engine with `wasm/build.sh`, which requires the `wasm32-unknown-unknown` target and `wasm-bindgen-cli`. Pass `--engine=dev` for faster unoptimized builds, or `--engine=none` to reuse a previously built engine in the output directory. `cargo site serve` defaults to `--engine=dev` and starts the server even if a documentation check fails.

## Verification

The site generator links directly against the `sandlang` crate so that documentation stays synchronized with the implementation:

- Guide and home page examples are formatted, type-checked, and executed during the build.
- Code blocks are syntax-highlighted with the Sand lexer.
- Library reference pages are generated from `Engine::docs`.
- Internal links and heading anchors are verified across all generated HTML and repository Markdown files.

## Directory Layout

| Path | Purpose |
| --- | --- |
| `content/index.md` | Home page content |
| `content/guide/*.md` | Guide chapters, ordered by filename (`NN-slug.md`) |
| `static/` | Stylesheet, site script, logo, and icons |
| `playground/` | Browser playground served at `/play/` ([README](playground/README.md)) |
| `src/` | Site generator and local HTTP server |
| `tests/` | Node.js test suite for playground examples |
| `icons.sh` | Script that renders PNG icons from `static/logo.svg` |

Content files begin with `+++` front matter:

```text
+++
title = "Errors"
description = "Summary used in search results and page metadata."
+++
```

## Code Blocks

Every fenced code block in verified Markdown files requires a language tag (`sand`, `policy`, `rust`, `js`, `sh`, or `text`).

- `policy` blocks are parsed as policy files.
- `sand` blocks must be complete programs formatted according to `sand fmt`. By default, each block is executed and its standard output is rendered below the code.

| Info string | Verification behavior |
| --- | --- |
| `sand` | Format-check, run, and render output |
| `sand no_run` | Format-check and type-check without running |
| `sand error` | Expect a compile error and render the diagnostic |
| `sand fails` | Expect a runtime error and render the error |
| `sand ignore` | Highlight only (skip format and type checks) |

Lines prefixed with `# ` are included when compiling and running an example, but hidden from the rendered code block. Examples run with a deterministic random seed (`42`), an empty in-memory file system, and the policy defined in `examples::POLICY` (read/write access to `/` and a 10-second time limit). Code blocks in doc comments and `docs/SPEC.md` are highlighted without execution.

`rust` blocks are compiled by `cargo test -p sandlang-site` via `build.rs`. A block that defines `fn main` is compiled as a standalone module; all other blocks are wrapped in an `async fn` returning `Result<(), Box<dyn std::error::Error>>`. Use `rust ignore` to skip compilation.

## Logo and Icons

`static/logo.svg` is the master logo. `static/favicon.svg` is a simplified variant optimized for small tab sizes. Run `icons.sh` with headless Chrome to regenerate `static/apple-touch-icon.png` and `static/og.png`.

## Deployment

Deploy `target/site` to any static file host that serves `index.html` for directory paths, `404.html` for missing routes, and `.wasm` files with `Content-Type: application/wasm`.
