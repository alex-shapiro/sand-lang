# Sand Playground

The browser playground lets you write Sand programs, configure sandbox policies, and run multiple isolated Sand engines concurrently in Web Workers.

## Local Development

The site generator serves the playground at `/play/` (see [`../README.md`](../README.md)):

```sh
cargo site serve
```

This command builds the WebAssembly engine and serves the site at `http://127.0.0.1:8000/play/`. During the build, the generator replaces `<!-- site-head -->` and `<!-- site-chrome -->` in `index.html` with the shared site header and metadata.

## Interface

- **Program and files:** The first file tab (`main.sand`) is the entry point. Click **+** to add a file into the in-memory file system (`/notes.txt`, `/data/sales.csv`), or double-click a tab to rename it. Programs can import `.sand` files from the virtual file system (`import "lib/util"`). Files written during execution appear below the console.
- **Policy, environment, and input:**
  - **Policy:** Defines capability rules and resource limits (`docs/SPEC.md` §14.7).
  - **Env:** Defines `NAME=value` environment variables.
  - **Input:** Provides standard input bytes to `io.stdin`.
- **Network:** Allowed `http` requests are executed via the browser `fetch` API. Target servers must permit CORS reads, and HTTP redirects are rejected.
- **Execution:** Click **▶** (`Ctrl-Enter` / `Cmd-Enter`) to start a new session and run the program. Click **■** to terminate the worker immediately.
- **Interactive console:** After a program finishes without calling `exit`, type expressions or statements at the `›` prompt to evaluate them in the same session. Spawned background tasks continue running between console inputs.
- **Checking and formatting:** A background worker type-checks and lints the program and policy as you type. Click **Format** (`Ctrl-S` / `Cmd-S`) to format the active `.sand` or policy tab.
- **Persistence and sharing:** Sandboxes are saved in `localStorage`. Click **Share** to encode all sandboxes into the URL `#share=` fragment.

## Architecture

- `app.js` — Compiles the WebAssembly module, manages sandbox UI views, and coordinates workers.
- `worker.js` — Hosts `Engine` and `Session` instances inside a Web Worker and forwards output and permission prompts to the main thread.
- `sandbox.js` — Defines the sandbox state model, `Engine` options conversion, and `#share=` URL encoding.
- `editor.js` — Implements the syntax-highlighted code editor and diagnostic overlays.
- `examples.js` — Defines built-in playground examples verified by `../tests/playground.test.mjs`.

The WebAssembly module is built without the `os` feature (`--no-default-features`), so scripts can access only the virtual file system, environment, streams, and browser `fetch` hooks provided by the page.
