// Tests of the WebAssembly build in Node.js:
//
//   wasm/build.sh nodejs target/wasm-node
//   node --test wasm/tests/
//
// SAND_WASM_PKG overrides the package directory.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const pkg = process.env.SAND_WASM_PKG ?? path.join(here, "../../target/wasm-node");
const sand = createRequire(import.meta.url)(path.join(pkg, "sandlang_wasm.js"));

/** Runs `source` in a new engine; returns the result and the output. */
async function run(source, options = {}, hooks = {}) {
  const out = { stdout: "", stderr: "" };
  const engine = new sand.Engine(options, { onOutput: (stream, text) => (out[stream] += text), ...hooks });
  try {
    const result = await engine.run(source);
    return { result, ...out, engine };
  } finally {
    // The caller may still read files.
  }
}

test("runs a script and streams its output", async () => {
  const { result, stdout, stderr } = await run('print("hello")\neprint("oops")\nprint(1 + 2)');
  assert.equal(result.ok, true);
  assert.equal(result.exitCode, 0);
  assert.equal(stdout, "hello\n3\n");
  assert.equal(stderr, "oops\n");
  assert.equal(typeof result.elapsedMs, "number");
});

test("output arrives while a script computes", async () => {
  let firstOutput = 0;
  const engine = new sand.Engine({}, { onOutput: () => (firstOutput ||= Date.now()) });
  const started = Date.now();
  // Prints, then computes for a while without printing.
  const result = await engine.run('print("started")\nvar i = 0\nwhile i < 30000000 { i += 1 }\nprint("done")');
  const finished = Date.now();
  assert.equal(result.ok, true);
  assert.ok(finished - started > 100, `the loop is long enough to tell (${finished - started} ms)`);
  assert.ok(firstOutput - started < (finished - started) / 2, `"started" came ${firstOutput - started} ms into a ${finished - started} ms run`);
});

test("reports compile errors with rendered diagnostics", async () => {
  const { result } = await run("let x: int = \"no\"");
  assert.equal(result.ok, false);
  assert.equal(result.exitCode, 2);
  assert.equal(result.error.kind, "compile");
  const d = result.error.diagnostics[0];
  assert.equal(d.severity, "error");
  assert.equal(d.line, 1);
  assert.match(d.rendered, /let x: int = "no"/);
});

test("reports errors with a trace and exit codes", async () => {
  const failure = await run("fn f() -> int { fail(\"boom\") }\nprint(f())");
  assert.equal(failure.result.exitCode, 1);
  assert.equal(failure.result.error.kind, "runtime");
  assert.equal(failure.result.error.message, "error: boom");
  assert.equal(failure.result.error.trace[0].function, "f");
  const exit = await run("exit(7)");
  assert.deepEqual([exit.result.ok, exit.result.exitCode], [false, 7]);
});

test("the policy decides file access in the memory file system", async () => {
  const source = `import fs
print(fs.read_text("/data/in.txt"))
print(fs.read_text("/data/secret.txt").is_ok())
print(fs.write_text("/out/result.txt", "written").is_ok())
print(fs.write_text("/data/in.txt", "clobbered").is_ok())
print(fs.list_dir("/data"))`;
  const options = {
    policy: "allow read /data\ndeny read /data/secret.txt\nallow write /out",
    files: { "/data/in.txt": "input", "/data/secret.txt": "secret", "/out/.keep": new Uint8Array() },
  };
  const { result, stdout, engine } = await run(source, options);
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(stdout, 'Ok("input")\nfalse\ntrue\nfalse\nOk(["in.txt", "secret.txt"])\n');
  assert.equal(new TextDecoder().decode(engine.readFile("/out/result.txt")), "written");
  assert.equal(new TextDecoder().decode(engine.readFile("/data/in.txt")), "input");
  const paths = engine.files().map((f) => f.path);
  assert.deepEqual(paths, ["/data/in.txt", "/data/secret.txt", "/out/.keep", "/out/result.txt"]);
});

test("relative paths and imports resolve in the memory file system", async () => {
  const options = {
    policy: "allow read ./data",
    files: { "/lib/util.sand": "pub fn twice(x: int) -> int { x * 2 }\n", "/data/n.txt": "21" },
  };
  const source = 'import "lib/util"\nimport fs\nlet n = fs.read_text("data/n.txt").unwrap().trim().parse_int().unwrap()\nprint(util.twice(n))';
  const { result, stdout } = await run(source, options);
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(stdout, "42\n");
});

test("modules that need the operating system are not available", async () => {
  const { result } = await run("import net");
  assert.equal(result.error.kind, "compile");
  assert.match(result.error.message, /module `net` is not available: this build of Sand has no operating-system access/);
});

test("limits from the policy stop runaway scripts", async () => {
  const loop = await run("var i = 0\nwhile true { i += 1 }", { policy: "limit instructions 100000" });
  assert.equal(loop.result.exitCode, 3);
  assert.equal(loop.result.error.kind, "limit");
  assert.match(loop.result.error.message, /instruction limit/);
  const memory = await run("var s = \"x\"\nwhile true { s = s + s }", { policy: "limit memory 1M" });
  assert.match(memory.result.error.message, /memory limit/);
  const time = await run("var i = 0\nwhile true { i += 1 }", { policy: "limit time 50ms" });
  assert.match(time.result.error.message, /time limit/);
});

test("counts past u32::MAX keep their meaning with a 32-bit usize", async () => {
  const BIG = "4294967297"; // 2^32 + 1: would wrap to 1
  for (const source of [
    `print("x".repeat(${BIG}).len())`,
    `print("x".pad_start(${BIG}).len())`,
    `import random\nprint(random.bytes(${BIG}).len())`,
    `print((0..${BIG}).to_list().len())`,
    `print((0..${BIG}).reversed().len())`,
    `print([0; ${BIG}].len())`,
  ]) {
    const { result } = await run(source, { policy: "limit memory 4M" });
    assert.equal(result.error?.kind, "limit", source);
    assert.match(result.error.message, /memory limit/, source);
  }
  const { result, stdout } = await run(`let xs = [1, 2, 3]
print(xs.get(${BIG}))
print("abc".char_at(${BIG}))
print("abc".bytes().get(${BIG}))
print(xs.take(${BIG}))
print(xs.skip(${BIG}))
print(xs.chunks(${BIG}))
print(xs.windows(${BIG}))
print((0..10000000000).step_by(5000000000))
let ch = Channel(capacity: ${BIG})
ch.send(1)
print(ch.len())`);
  assert.equal(result.ok, true, JSON.stringify(result.error));
  assert.equal(stdout, "None\nNone\nNone\n[1, 2, 3]\n[]\n[[1, 2, 3]]\n[]\n[0, 5000000000]\n1\n");
  for (const source of [`print([1, 2, 3][${BIG}])`, `var xs = [1, 2, 3]\nxs[${BIG}] = 4`, `var xs = [1, 2, 3]\nxs.insert(${BIG}, 4)`]) {
    const { result } = await run(source);
    assert.equal(result.error?.kind, "runtime", source);
    assert.match(result.error.message, new RegExp(`index out of bounds: index ${BIG}, length 3`), source);
  }
});

test("environment, arguments, standard input and seeds come from the host", async () => {
  const source = `import env
import io
import random
print(env.get("NAME"))
print(env.get("HIDDEN"))
print(env.args())
print(io.stdin.read_line())
print(random.int(0, 1000000))`;
  const options = { policy: "allow env NAME\nallow stdin", env: { NAME: "sand", HIDDEN: "x" }, args: ["a", "b"], stdin: "line\n", seed: 42 };
  const first = await run(source, options);
  assert.equal(first.result.ok, true, JSON.stringify(first.result));
  const lines = first.stdout.split("\n");
  assert.deepEqual(lines.slice(0, 4), ['Some("sand")', "None", '["a", "b"]', 'Ok(Some("line"))']);
  const second = await run(source, options);
  assert.equal(second.stdout, first.stdout, "seeded runs are reproducible");
});

test("timers wait without blocking, and tasks run concurrently", async () => {
  const source = `let a = spawn { task.sleep(30ms)\n print("a") }
let b = spawn { task.sleep(10ms)\n print("b") }
a.wait()
b.wait()
print("done")`;
  const started = Date.now();
  const { result, stdout } = await run(source);
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(stdout, "b\na\ndone\n");
  assert.ok(Date.now() - started >= 25);
});

test("several engines run at once", async () => {
  const program = (name, ms) => `for i in 0..3 {\n task.sleep(${ms}ms)\n print("${name}\\(i)") }`;
  const runs = await Promise.all([run(program("x", 7)), run(program("y", 11)), run(program("z", 3))]);
  assert.deepEqual(
    runs.map((r) => r.stdout),
    ["x0\nx1\nx2\n", "y0\ny1\ny2\n", "z0\nz1\nz2\n"],
  );
});

test("an engine runs one script at a time", async () => {
  const engine = new sand.Engine({}, {});
  const first = engine.run("task.sleep(10ms)");
  await assert.rejects(engine.run("print(1)"), /busy/);
  assert.equal((await first).ok, true);
  assert.equal((await engine.run("print(1)")).ok, true);
});

test("interrupting stops a waiting run", async () => {
  const engine = new sand.Engine({}, {});
  const pending = engine.run("task.sleep(10s)");
  setTimeout(() => engine.interrupt(), 20);
  const result = await pending;
  assert.deepEqual([result.exitCode, result.error.kind], [130, "interrupted"]);
});

test("prompts go to onPrompt", async () => {
  const source = `import fs
print(fs.read_text("/a.txt").is_ok())
print(fs.read_text("/a.txt").is_ok())
print(fs.read_text("/b.txt").is_ok())`;
  const asked = [];
  const answers = { "/a.txt": "once", "/b.txt": "deny" };
  const onPrompt = async (request) => {
    asked.push(request);
    await new Promise((r) => setTimeout(r, 1));
    return answers[request.target];
  };
  const { result, stdout } = await run(source, { files: { "/a.txt": "A", "/b.txt": "B" } }, { onPrompt });
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(stdout, "true\ntrue\nfalse\n");
  assert.deepEqual(
    asked.map((r) => [r.kind, r.target, r.description]),
    [
      ["read", "/a.txt", "read /a.txt"],
      ["read", "/a.txt", "read /a.txt"],
      ["read", "/b.txt", "read /b.txt"],
    ],
  );
  // "always" is remembered by the engine; a failing callback denies.
  const always = await run(source, { files: { "/a.txt": "A", "/b.txt": "B" } }, { onPrompt: () => "always" });
  assert.equal(always.stdout, "true\ntrue\ntrue\n");
  const broken = await run(source, { files: { "/a.txt": "A" } }, { onPrompt: () => { throw new Error("no"); } });
  assert.equal(broken.stdout, "false\nfalse\nfalse\n");
});

test("policies are formatted and linted", () => {
  assert.equal(sand.formatPolicy("allow  read /a,/b   # x\ndeny ~/.ssh"), "allow read /a /b  # x\ndeny ~/.ssh\n");
  assert.deepEqual(sand.lintPolicy("allow /data\nallow read ./data/x", "/"), [
    { code: "redundant-rule", message: "`/data` on line 1 already allows this", line: 2, column: 12 },
  ]);
  assert.throws(() => sand.lintPolicy("allow read ~/x"), (e) => e.policyErrors[0].message.startsWith("cannot resolve `~`"));
});

test("invalid policies and options are reported", () => {
  assert.throws(
    () => new sand.Engine({ policy: "allow read /x\nalow net *\nlimit memory lots" }, {}),
    (e) => {
      assert.deepEqual(e.policyErrors, [
        { line: 2, column: 1, message: "unknown directive `alow`: expected `allow`, `deny`, `prompt`, `limit`, or `let`" },
        { line: 3, column: 7, message: "invalid size `lots`: use e.g. 4096, 64K, 256M, or 1G" },
      ]);
      return /invalid policy/.test(e.message);
    },
  );
  assert.throws(() => new sand.Engine({ polcy: "" }, {}), /unknown engine option `polcy`/);
  assert.throws(() => new sand.Engine({}, { onOutpt() {} }), /unknown engine hook `onOutpt`/);
  assert.throws(() => new sand.Engine({}, { onOutput: 3 }), /`onOutput` must be a function/);
});

test("check, lint and format", () => {
  const engine = new sand.Engine({}, {});
  const ok = engine.check("let x = 1\nprint(x)");
  assert.deepEqual([ok.ok, ok.diagnostics.length, ok.lint], [true, 0, []]);
  const bad = engine.check("print(nope)");
  assert.equal(bad.ok, false);
  assert.match(bad.diagnostics[0].message, /nope/);
  assert.deepEqual(engine.check("let unused = 1\n").lint.map((w) => w.code), ["unused-variable"]);
  const warnings = engine.lint("let unused = 1\n");
  assert.ok(warnings.some((w) => /unused/.test(w.message)), JSON.stringify(warnings));
  assert.equal(sand.format("fn  f( )->int{1}"), "fn f() -> int { 1 }\n");
  assert.throws(() => sand.format("fn ("), /expected/);
  assert.equal(typeof sand.version(), "string");
});

test("files changed since a generation", async () => {
  const engine = new sand.Engine({ policy: "allow write /", files: { "/a.txt": "a", "/b.txt": "b" } }, {});
  const start = engine.generation();
  assert.deepEqual(engine.changedFiles(start), []);
  const result = await engine.run('import fs\nfs.write_text("/b.txt", "B")?\nfs.write_text("/d.txt", "D")?');
  assert.equal(result.ok, true, JSON.stringify(result));
  const changed = engine.changedFiles(start).map(({ path, data }) => [path, new TextDecoder().decode(data)]);
  assert.deepEqual(changed, [["/b.txt", "B"], ["/d.txt", "D"]]);
});

test("errors include their exit code", async () => {
  const { result } = await run('fail("boom")');
  assert.deepEqual([result.exitCode, result.error.exitCode, result.error.kind], [1, 1, "runtime"]);
  const session = new sand.Engine({}, {}).session();
  assert.equal((await session.eval("nope")).error.exitCode, 2);
});

test("sessions keep state between inputs", async () => {
  const out = [];
  const engine = new sand.Engine({}, { onOutput: (_, t) => out.push(t) });
  const session = engine.session();
  assert.deepEqual(await session.eval("var count = 0\nfn bump() -> int { count += 1\n count }"), {});
  assert.deepEqual(await session.eval("bump()"), { value: "1" });
  assert.deepEqual(await session.eval("bump() + 10"), { value: "12" });
  assert.equal(session.typeOf("bump"), "fn() -> int");
  const failed = await session.eval("nope");
  assert.equal(failed.error.kind, "compile");
  await session.eval('print("from session")');
  assert.deepEqual(out, ["from session\n"]);
  assert.equal(sand.Session.isComplete("fn f() {"), false);
  assert.equal(sand.Session.isComplete("fn f() {}"), true);
});

test("deeply nested programs are rejected without exhausting the stack", async () => {
  for (const [open, close] of [["(", ")"], ["[", "]"], ["{", "}"], ["-", ""]]) {
    const n = 100000;
    const { result } = await run(`let v = ${open.repeat(n)}1${close.repeat(n)}`);
    assert.equal(result.error.kind, "compile", `${open}: ${JSON.stringify(result.error).slice(0, 200)}`);
  }
  const chain = await run(`let v = ${"1 + ".repeat(100000)}1`);
  assert.ok(chain.result.error, "a long chain is rejected or compiled");
  const ifs = await run(`${"if true { ".repeat(5000)}1${" }".repeat(5000)}`);
  assert.ok(ifs.result.error);
  // The deepest program that is accepted still compiles and runs.
  const deep = await run(`print(${"(".repeat(200)}1${")".repeat(200)})`);
  assert.equal(deep.stdout, "1\n", JSON.stringify(deep.result));
});

/** A local HTTP server for the `http` module's tests; resolves to its port. */
function server() {
  const srv = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      if (req.url === "/hello") {
        res.writeHead(200, { "content-type": "text/plain", "x-greeting": "hi" });
        res.end("hello from the server");
      } else if (req.url === "/echo") {
        res.writeHead(201, { "content-type": "application/json" });
        res.end(JSON.stringify({ method: req.method, body, type: req.headers["content-type"] }));
      } else if (req.url === "/redirect") {
        res.writeHead(302, { location: "/hello" });
        res.end();
      } else if (req.url === "/big") {
        res.writeHead(200);
        res.end("x".repeat(100000));
      } else if (req.url === "/slow") {
        setTimeout(() => res.end("late"), 2000);
      } else {
        res.writeHead(404);
        res.end("nope");
      }
    });
  });
  return new Promise((resolve) => srv.listen(0, "127.0.0.1", () => resolve({ port: srv.address().port, close: () => srv.close() })));
}

test("http goes through fetch, as the policy allows", async () => {
  const { port, close } = await server();
  try {
    const base = `http://127.0.0.1:${port}`;
    const source = `import http
let r = http.get("${base}/hello")?
print("\\(r.status) \\(r.text()?) \\(r.header("x-greeting"))")
let p = http.post("${base}/echo", b"data", headers: [("content-type", "text/plain")])?
print("\\(p.status) \\(p.json()?)")
print(http.get("${base}/missing")?.status)
for url in ["${base}/redirect", "${base}/big", "http://example.com/", "ftp://x/"] {
    match http.get(url) {
        Ok(r) => print("ok \\(r.status)")
        Err(e) => print("\\(e.kind): \\(e.message)")
    }
}
print(http.get("${base}/slow", timeout: 50ms).unwrap_err().kind)
{
    use s = http.open("GET", "${base}/hello")?
    print("\\(s.status) \\(s.body.read(5)?) \\(s.body.read_text()?)")
}
`;
    const policy = `allow net plaintext 127.0.0.1:${port}\nlimit http-body 10K`;
    const { result, stdout } = await run(source, { policy });
    assert.equal(result.ok, true, JSON.stringify(result));
    const lines = stdout.trim().split("\n");
    assert.equal(lines[0], "200 hello from the server Some(\"hi\")");
    assert.equal(lines[1], '201 Obj(["method": Str("POST"), "body": Str("data"), "type": Str("text/plain")])');
    assert.equal(lines[2], "404");
    assert.match(lines[3], /^http: .*redirects elsewhere, and redirects are not followed in the browser/);
    assert.match(lines[4], /^limit: response body larger than 10240 bytes/);
    assert.equal(lines[5], "permission_denied: permission denied: GET http://example.com/");
    assert.match(lines[6], /^invalid: unsupported URL scheme/);
    assert.equal(lines[7], "timeout");
    assert.equal(lines[8], '200 b"hello"  from the server');
  } finally {
    close();
  }
});

test("http requests are prompted for and denied without a rule", async () => {
  const { port, close } = await server();
  try {
    const source = `import http\nprint(http.get("http://127.0.0.1:${port}/hello").map(r => r.status))`;
    const denied = await run(source, { policy: "" });
    assert.match(denied.stdout, /permission_denied/);
    const wildcard = await run(source, { policy: "allow net *" });
    assert.match(wildcard.stdout, /permission_denied/);
    const plaintext = await run(source, { policy: "allow net plaintext *" });
    assert.equal(plaintext.stdout, "Ok(200)\n");
    const asked = [];
    const prompted = await run(source, { policy: "" }, { onPrompt: (r) => (asked.push(r.description), "once") });
    assert.equal(prompted.stdout, "Ok(200)\n");
    assert.deepEqual(asked, [`GET http://127.0.0.1:${port}/hello`]);
    const routed = await run(source, { policy: `allow http plaintext GET 127.0.0.1:${port}/hello` });
    assert.equal(routed.stdout, "Ok(200)\n");
    const otherPath = await run(source, { policy: `allow http plaintext GET 127.0.0.1:${port}/other` });
    assert.match(otherPath.stdout, /permission_denied/);
  } finally {
    close();
  }
});

test("sessions continue a program, and run its tasks between inputs", async () => {
  let out = "";
  const engine = new sand.Engine({}, { onOutput: (_, t) => (out += t) });
  const session = engine.session();
  const program = "var ticks = 0\nlet ticker = spawn {\n    for i in 0..3 {\n        task.sleep(10ms)\n        ticks += 1\n        print(\"tick \\(ticks)\")\n    }\n}\nprint(\"started\")";
  assert.deepEqual(await session.eval(program, "main.sand"), {});
  assert.equal(out, "started\n");
  const tasks = session.runTasks();
  await new Promise((r) => setTimeout(r, 80));
  session.stopTasks();
  assert.deepEqual(await tasks, {});
  assert.equal(out, "started\ntick 1\ntick 2\ntick 3\n");
  assert.deepEqual(await session.eval("ticks * 10"), { value: "30" });
  // Errors name the program's file.
  const failed = await session.eval("let x: int = \"no\"", "main.sand");
  assert.equal(failed.error.diagnostics[0].file, "main.sand");
  // A failing task is reported, and exit in a task ends runTasks.
  await session.eval("spawn { task.sleep(5ms)\n fail(\"boom\") }");
  out = "";
  const failing = session.runTasks();
  await new Promise((r) => setTimeout(r, 40));
  session.stopTasks();
  await failing;
  assert.match(out, /boom[\s\S]*in a background task/);
  await session.eval("spawn { task.sleep(5ms)\n exit(4) }");
  assert.deepEqual(await session.runTasks(), { exitCode: 4 });
});

test("waiting for prompt answers does not count against the time limit", async () => {
  const slowYes = () => new Promise((resolve) => setTimeout(() => resolve("once"), 600));
  const source = 'import fs\nprint(fs.read_text("/a.txt").is_ok())\nprint(fs.read_text("/a.txt").is_ok())';
  const { result, stdout } = await run(source, { policy: "limit time 200ms", files: { "/a.txt": "A" } }, { onPrompt: slowYes });
  assert.equal(result.ok, true, JSON.stringify(result));
  assert.equal(stdout, "true\ntrue\n");
});

test("prompt requests include the allow and deny rules", async () => {
  const requests = [];
  const source = 'import env\nimport fs\nprint(env.get("HOME"))\nprint(fs.read_text("/my notes.txt").is_ok())';
  await run(source, { files: { "/my notes.txt": "x" } }, { onPrompt: (r) => (requests.push(r), "deny") });
  assert.deepEqual(
    requests.map((r) => [r.allowRule, r.denyRule]),
    [
      ["allow env HOME", "deny env HOME"],
      ['allow read "/my notes.txt"', 'deny read "/my notes.txt"'],
    ],
  );
  const asked = [];
  const { stdout } = await run(source, { policy: requests.map((r) => r.allowRule).join("\n"), files: { "/my notes.txt": "x" }, env: { HOME: "/home/sand" } }, { onPrompt: (r) => (asked.push(r), "deny") });
  assert.deepEqual(asked, []);
  assert.equal(stdout, 'Some("/home/sand")\ntrue\n');
});

test("requests the browser blocks say so", async () => {
  const { stdout } = await run('import http\nlet e = http.get("http://127.0.0.1:1/").unwrap_err()\nprint("\\(e.kind): \\(e.message)")', { policy: "allow net plaintext 127.0.0.1:1" });
  assert.equal(stdout, "io: the browser blocked the request to http://127.0.0.1:1/: 127.0.0.1:1 does not let pages of other sites read its answers (CORS), or it cannot be reached\n");
});
