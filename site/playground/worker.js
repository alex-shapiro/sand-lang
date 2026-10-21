// Web Worker that executes Sand engines and interactive sessions off the main thread.

import { initSync, Engine, format, formatPolicy, isTimeZone, lintPolicy } from "./pkg/sandlang_wasm.js";
import { engineOptions, program } from "./sandbox.js";

const pendingPrompts = new Map();
let nextPrompt = 1;
let queue = Promise.resolve();
let current = null;

self.onmessage = (event) => {
  const message = event.data;
  if (message.type === "init") {
    try {
      initSync({ module: message.module });
      self.postMessage({ type: "ready" });
    } catch (e) {
      self.postMessage({ type: "failed", message: String(e?.message ?? e) });
    }
  } else if (message.type === "answer") {
    pendingPrompts.get(message.promptId)?.(message.answer);
    pendingPrompts.delete(message.promptId);
  } else {
    queue = queue.then(() => handle(message)).catch((e) => {
      const reason = self.sandPanic ?? String(e?.message ?? e);
      self.postMessage({ type: "error", id: message.id, message: reason });
    });
  }
};

async function handle(message) {
  const { id } = message;
  switch (message.type) {
    case "run":
      return run(id, message.sandbox);
    case "input":
      return input(id, message.source);
    case "check":
      return check(id, message.sandbox);
    case "lint":
      return lint(id, message.sandbox);
    case "format":
      try {
        const text = message.policy ? formatPolicy(message.source) : format(message.source);
        self.postMessage({ type: "formatted", id, text });
      } catch (e) {
        self.postMessage({ type: "formatted", id, error: String(e?.message ?? e) });
      }
  }
}

const hooks = {
  onOutput: (stream, text) => self.postMessage({ type: "output", stream, text }),
  onPrompt: (request) =>
    new Promise((resolve) => {
      const promptId = nextPrompt++;
      pendingPrompts.set(promptId, resolve);
      self.postMessage({ type: "prompt", promptId, request });
    }),
};

function engineFor(sandbox, engineHooks = {}) {
  const options = engineOptions(sandbox);
  const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  if (isTimeZone(zone)) options.timeZone = zone;
  try {
    return { engine: new Engine(options, engineHooks) };
  } catch (e) {
    if (e?.policyErrors) return { policyErrors: e.policyErrors };
    throw e;
  }
}

async function endSession() {
  if (!current) return;
  const { engine, session } = current;
  await pauseTasks();
  current = null;
  session.free();
  engine.free();
}

async function pauseTasks() {
  if (!current?.tasks) return;
  current.session.stopTasks();
  await current.tasks;
  current.tasks = null;
}

function resumeTasks() {
  const session = current;
  session.tasks = session.session.runTasks().then((r) => {
    if (r.exitCode !== undefined && current === session) {
      self.postMessage({ type: "ended", exitCode: r.exitCode });
    }
  });
}

async function run(id, sandbox) {
  await endSession();
  const { engine, policyErrors } = engineFor(sandbox, hooks);
  if (!engine) {
    const message = "The policy has errors:\n" + policyErrors.map((e) => `  line ${e.line}: ${e.message}`).join("\n");
    const result = { ok: false, exitCode: 2, error: { kind: "policy", message, diagnostics: [], trace: [] } };
    self.postMessage({ type: "done", id, result, written: [], policyErrors });
    return;
  }
  const session = engine.session();
  current = { engine, session, sandbox, tasks: null, start: engine.generation() };
  const evaluated = await session.eval(sandbox.files[program(sandbox)] ?? "", program(sandbox));
  const result = asRunResult(evaluated);
  const written = engine.changedFiles(current.start);
  self.postMessage({ type: "done", id, result, written }, written.map((f) => f.data.buffer));
  if (evaluated.exitCode === undefined) resumeTasks();
}

async function input(id, source) {
  if (!current) {
    self.postMessage({ type: "evaluated", id, result: { error: { kind: "io", message: "Run the program to start a session first.", diagnostics: [], trace: [] } }, written: [] });
    return;
  }
  await pauseTasks();
  const result = await current.session.eval(source);
  const written = current.engine.changedFiles(current.start);
  self.postMessage({ type: "evaluated", id, result, written }, written.map((f) => f.data.buffer));
  if (result.exitCode === undefined) resumeTasks();
}

function asRunResult(evaluated) {
  if (evaluated.error) {
    return { ok: false, exitCode: evaluated.exitCode ?? evaluated.error.exitCode, error: evaluated.error };
  }
  const exitCode = evaluated.exitCode ?? 0;
  return { ok: exitCode === 0, exitCode, value: evaluated.value };
}

function check(id, sandbox) {
  const { engine, policyErrors } = engineFor(sandbox);
  if (!engine) {
    self.postMessage({ type: "checked", id, policyErrors });
    return;
  }
  try {
    const name = program(sandbox);
    const result = engine.check(sandbox.files[name] ?? "", name);
    const policyWarnings = lintPolicy(sandbox.policy ?? "");
    self.postMessage({ type: "checked", id, check: result, lint: result.lint, policyErrors: [], policyWarnings });
  } finally {
    engine.free();
  }
}

function lint(id, sandbox) {
  const { engine, policyErrors } = engineFor(sandbox);
  if (!engine) {
    self.postMessage({ type: "linted", id, lint: [], error: `The policy has errors (line ${policyErrors[0].line}: ${policyErrors[0].message})` });
    return;
  }
  try {
    self.postMessage({ type: "linted", id, lint: engine.lint(sandbox.files[program(sandbox)] ?? "", program(sandbox)) });
  } catch (e) {
    self.postMessage({ type: "linted", id, lint: [], error: String(e?.message ?? e) });
  } finally {
    engine.free();
  }
}
