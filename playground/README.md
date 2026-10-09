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
--port=PORT` serves another copy or uses another port. The page must be
served over HTTP(S), not opened as a file (browsers do not start its
workers or load its WebAssembly from `file://` pages), but nothing runs on
the server: any static file server works (`python3 -m http.server -d
playground`), and to host it, copy the directory (with `pkg/`) to any static
host. It needs a browser with module workers (current Chrome, Edge, Firefox
and Safari) and works offline once loaded.

## Using it

- **Program and files.** The first file tab (`main.sand`) is the program.
  The **+** after the last tab adds a file; double-click a tab to rename it
  (`notes.txt`, `data/sales.csv`, `lib/util.sand`; the program too). The
  program sees the other files at `/notes.txt`, `/data/sales.csv`, ..., and
  `.sand` files can be imported (`import "lib/util"`). Files the program
  writes are listed below the console; open one to see it in a tab.
- **Policy, environment, input.** The darker Policy, Env and Input tabs left
  of the files. The policy is a policy file (`docs/SPEC.md` §14.7) saying
  what the program may and may not do (`allow read /data`, `deny read
  /data/private`, `allow net api.github.com`, `allow env USER`, `allow
  stdin`) and its limits (`limit time 2s`). Whatever the policy does not
  decide, the program asks you about in the page when it needs it: allow or
  deny once, or always ("Always" and "Never" add the rule that says so to
  the policy, e.g. `allow read /notes/todo.txt`, so later runs do not ask).
  Waiting for your answer does not count against time limits. The environment holds `NAME=value` lines for
  `env.get`; the input is the program's standard input.
- **Network.** `http` works in the browser, through `fetch`, to the hosts the
  policy allows or you allow when asked (nothing is sent otherwise).
  The site must allow pages to read its responses (CORS), as public APIs
  such as `api.github.com` do; redirects are not followed.
- **Running.** ▶ (Ctrl-Enter / ⌘-Enter) runs the program in a new session,
  each sandbox in its own engine and worker, so several can run at once.
  ■ stops a run at once (its worker is terminated). Output streams in as
  the program produces it.
- **The console.** It appears once there is output. After a run, type at
  its `›` prompt to continue the program: expressions (their values are
  shown), statements and declarations, in the context of the program's variables, functions and
  heap. Enter runs a complete input (an open bracket continues it on the
  next line); Up and Down recall earlier inputs. Tasks the program spawned
  keep running between inputs. Run again to start over.
- **Checking.** As you type, the program is type checked and linted and the
  policy parsed, off the page's thread; problems are marked in the editor.
  Hover a mark or its line number to read it; when the caret is on a line
  with problems, they also show below the editor.
  Lint lists the linter's warnings in the console; Format (Ctrl-S / ⌘-S)
  formats the open `.sand` file.
- **Keeping and sharing.** Sandboxes are kept in the browser's local storage
  (the trash button deletes one, with an undo). Share copies a link that
  holds every sandbox (in the URL's fragment, never sent to a server).

## How it works

- `app.js` — the page. It compiles the WebAssembly module once
  (`pkg/sandlang_wasm_bg.wasm`), uses it on the page only for syntax
  highlighting (the Sand lexer, through `highlight`), and hands it to the
  workers.
- `worker.js` — runs engines: one worker per sandbox for its session, and one
  shared worker for checking, linting and formatting. A run makes `new
  Engine(options, hooks).session()` and evaluates the program as its first
  input (`session.eval(source, "main.sand")`); console inputs are further
  `eval`s, and between them `session.runTasks()` keeps the program's tasks
  running. Output and permission prompts are posted to the page as they
  happen.
- `sandbox.js` — what a sandbox is, without the page: its files, policy,
  environment and input, the engine options they become, and share links.
- `editor.js` — the code editor: a textarea over a highlighted copy of its
  text, with line numbers and diagnostics.
- `examples.js` — the examples of the "Open an example…" menu.
  `wasm/tests/playground.test.mjs` runs each one with the WebAssembly build
  and checks its outcome.

The engine is the same as the command line's, built without operating-system
access (`--no-default-features`): scripts see only the files, environment and
input the page gives them; `http` goes through the browser's `fetch` as the
policy allows, and `net` (raw sockets) does not exist.
Limits from the policy (memory, time, instructions, ...) stop runaway scripts;
Stop stops the rest.
