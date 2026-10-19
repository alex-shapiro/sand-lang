# Sand playground

A web page to try Sand: write a program and the policy that sandboxes it
side by side, and run it. Every sandbox on the page is a separate Sand
engine in its own Web Worker, so several run at the same time.

## Running it

```
playground/build.sh    # the engine, into playground/pkg (needs wasm-bindgen-cli)
sand playground        # serves this directory at http://127.0.0.1:8000/
```

`build.sh --dev` builds faster and runs slower. `sand playground DIR
--port=PORT` serves another copy or uses another port. Any static file
server works (`python3 -m http.server -d playground`), but not `file://`
pages; to host it, copy the directory (with `pkg/`) to a static host. It
needs a browser with module workers and works offline after it loads.
`sand playground` sends `Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp`. With these headers, ■ stops a
busy program and keeps its worker. Without them, ■ terminates the worker.

## Using it

- **Program and files.** The first file tab (`main.sand`) is the program.
  **+** adds a file; double-click a tab to rename it. The program sees the
  other files at `/notes.txt`, `/data/sales.csv`, ..., and can import `.sand`
  files (`import "lib/util"`). Files it writes are listed below the console.
- **Policy, environment, input.**
  - The Policy tab contains rules defining the system resources that a sandbox engine can access (`docs/SPEC.md` §14.7).
  - The Env tab defines environment variables
  - The Input tab contains the program's standard input.
- **Network.** `http` executes via `fetch` for allowed requests.
  The site must allow CORS reads; redirects are not followed.
- **Running.** ▶ (Ctrl-Enter / ⌘-Enter) runs the program in a new session;
  ■ stops it at once and ends the session. Output streams in.
- **The console.** After a run, type at its `›` prompt to continue the
  program with its variables, functions and heap. Enter runs a complete
  input; Up and Down recall earlier inputs. Tasks the program spawned keep
  running between inputs. Run again to start over.
- **Checking.** As you type, the program is type checked and linted and the
  policy parsed and linted, off the page's thread. Hover over a mark or its
  line number to inspect a problem. Linter warnings are printed in the console;
  Format (Ctrl-S / ⌘-S) formats the open `.sand` or `.policy` file.
- **Keeping and sharing.** Sandboxes are kept in local storage. Share copies
  a link holding every sandbox in its fragment, never sent to a server.

## How it works

- `app.js` — the page. It compiles the WebAssembly module once, uses it
  only for syntax highlighting, and hands it to the workers.
- `worker.js` — runs engines: one worker per sandbox for its session, and one
  shared worker for checking, linting and formatting. A run makes `new
Engine(options, hooks).session()` and evaluates the program as its first
  input (`session.eval(source, "main.sand")`); console inputs are further
  `eval`s, and between them `session.runTasks()` keeps the program's tasks
  running. Output and permission prompts are posted to the page as they
  happen. To stop a session, the page stores 1 in the `interruptBuffer`
  that it shares with the worker. A busy engine reads this buffer. The page
  also sends `stop`, which interrupts an engine that waits. If the page is
  not cross-origin isolated, or if the run does not stop in one second, the
  page terminates the worker.
- `sandbox.js` — what a sandbox is, without the page: its files, policy,
  environment and input, the engine options they become, and share links.
- `editor.js` — the code editor: a textarea over a highlighted copy of its
  text, with line numbers and diagnostics.
- `examples.js` — the examples, which `wasm/tests/playground.test.mjs` runs.

The engine is the command line's, built without operating-system access
(`--no-default-features`): scripts see only what the page gives them, and
`net` (raw sockets) does not exist.
