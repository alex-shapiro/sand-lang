// Executes every example in site/playground/examples.js against the Node.js
// WebAssembly build using a stubbed `fetch` implementation.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { examples } from "../playground/examples.js";
import { engineOptions, MAIN } from "../playground/sandbox.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const pkg = process.env.SAND_WASM_PKG ?? path.join(here, "../../target/wasm-node");
const sand = createRequire(import.meta.url)(path.join(pkg, "sandlang_wasm.js"));

const answers = {
  "https://api.github.com/zen": "Keep it logically awesome.",
  "https://api.github.com/repos/rust-lang/rust": JSON.stringify({
    full_name: "rust-lang/rust",
    description: "Empowering everyone to build reliable and efficient software.",
    stargazers_count: 100000,
    language: "Rust",
    id: 724712,
  }),
  "https://en.wikipedia.org/api/rest_v1/page/summary/Sand": JSON.stringify({
    title: "Sand",
    extract: "Sand is a granular material composed of finely divided mineral particles. It is defined by size.",
  }),
};
const requested = [];
globalThis.fetch = async (url) => {
  requested.push(url);
  if (!(url in answers)) throw new TypeError("Failed to fetch");
  return new Response(answers[url], { status: 200, headers: { "content-type": "application/json" } });
};

for (const example of examples) {
  test(`example: ${example.title}`, async () => {
    let stdout = "";
    let stderr = "";
    let prompts = 0;
    requested.length = 0;
    const engine = new sand.Engine(engineOptions(example), {
      onOutput: (stream, text) => (stream === "stdout" ? (stdout += text) : (stderr += text)),
      onPrompt: () => {
        prompts += 1;
        return example.expect.answer ?? "deny";
      },
    });
    const source = example.files[MAIN];
    const check = engine.check(source);
    assert.equal(check.ok, true, `${example.id} does not type check: ${JSON.stringify(check.diagnostics)}`);
    assert.deepEqual(engine.lint(source), [], `${example.id} has lint warnings`);
    const session = engine.session();
    const result = await session.eval(source, MAIN);
    const e = example.expect;
    const context = `${example.id}: ${JSON.stringify(result)}\n--- stdout\n${stdout}--- stderr\n${stderr}`;
    assert.equal(!result.error && (result.exitCode ?? 0) === 0, e.ok, context);
    if (e.error) assert.match(result.error.message, new RegExp(e.error), context);
    for (const text of [e.stdout ?? []].flat()) assert.ok(stdout.includes(text), `missing ${JSON.stringify(text)}\n${context}`);
    if (e.prompts !== undefined) assert.equal(prompts, e.prompts, context);
    for (const written of e.writes ?? []) {
      assert.ok(engine.readFile(written), `${example.id} did not write ${written}`);
    }
    for (const url of requested) assert.ok(url in answers, `${example.id} requested ${url}`);
    if (e.ok) assert.equal(stderr, "", context);
    session.free();
    engine.free();
  });
}

test("the welcome example's session continues", async () => {
  const welcome = examples.find((e) => e.id === "welcome");
  let stdout = "";
  const engine = new sand.Engine(engineOptions(welcome), { onOutput: (_, t) => (stdout += t) });
  const session = engine.session();
  assert.equal((await session.eval(welcome.files[MAIN], MAIN)).error, undefined);
  assert.deepEqual(await session.eval("planets.len()"), { value: "5" });
  assert.deepEqual(await session.eval("size(planets[4])"), { value: "Many(146)" });
});
