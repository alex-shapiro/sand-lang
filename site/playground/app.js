// Browser playground UI: manages sandbox editors, worker sessions, and state persistence.

import { CodeEditor, highlightPolicy } from "./editor.js";
import { examples } from "./examples.js";
import { newSandbox, program, encodeShare, decodeShare } from "./sandbox.js";

const STORAGE_KEY = "sand-playground/v1";
const CHECK_DELAY = 300;
const MAX_CONSOLE = 2 << 20;
const MAX_HISTORY = 200;

const $ = (selector, root = document) => root.querySelector(selector);

async function compileModule(url) {
  try {
    return await WebAssembly.compileStreaming(fetch(url));
  } catch {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`cannot load ${url}: ${response.status} ${response.statusText}`);
    return WebAssembly.compile(await response.arrayBuffer());
  }
}

class EngineWorker {
  constructor(module, onEvent = () => {}) {
    this.worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
    this.requests = new Map();
    this.nextId = 1;
    this.onEvent = onEvent;
    this.ready = new Promise((resolve, reject) => {
      this.onReady = resolve;
      this.onFailed = reject;
    });
    this.worker.onmessage = (e) => this.receive(e.data);
    this.worker.onerror = (e) => this.fail(e.message || "the worker failed to start");
    this.worker.postMessage({ type: "init", module });
  }

  receive(message) {
    if (message.type === "ready") return this.onReady();
    if (message.type === "failed") return this.fail(message.message);
    if (message.id === undefined) {
      return this.onEvent(message, (answer) => this.worker.postMessage({ type: "answer", promptId: message.promptId, answer }));
    }
    const request = this.requests.get(message.id);
    if (!request) return;
    this.requests.delete(message.id);
    if (message.type === "error") request.reject(new Error(message.message));
    else request.resolve(message);
  }

  fail(reason) {
    this.onFailed(new Error(reason));
    for (const r of this.requests.values()) r.reject(new Error(reason));
    this.requests.clear();
  }

  async request(message) {
    await this.ready;
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.requests.set(id, { resolve, reject });
      this.worker.postMessage({ ...message, id });
    });
  }

  terminate() {
    this.worker.terminate();
    for (const r of this.requests.values()) r.reject(new Error("stopped"));
    this.requests.clear();
  }
}

const SETTINGS = [
  {
    id: "policy",
    label: "Policy",
    placeholder: "# Capability rules and resource limits.\n# allow read /data\n# deny read /data/private\n# allow net api.github.com\n# limit time 5s",
    hint: "Capability rules (allow/deny) and resource limits. Unmatched operations prompt for permission.",
  },
  {
    id: "env",
    label: "Env",
    placeholder: "NAME=value, one per line",
    hint: "Environment variables (NAME=value per line), accessible via env.get if allowed by `allow env NAME`.",
  },
  {
    id: "stdin",
    label: "Input",
    placeholder: "Standard input",
    hint: "Standard input read by io.stdin if allowed by `allow stdin`.",
  },
];

class SandboxView {
  constructor(app, model) {
    this.app = app;
    this.model = model;
    this.tab = { kind: "file", name: program(model) };
    this.diagnostics = {};
    this.policyErrors = [];
    this.policyWarnings = [];
    this.running = null;
    this.worker = null;
    this.session = false;
    this.history = [];
    this.root = $("#sandbox-template").content.firstElementChild.cloneNode(true);
    this.consoleEl = $(".console", this.root);
    this.title = $(".title", this.root);
    this.title.value = model.title;
    this.fitTitle();
    this.title.addEventListener("input", () => {
      model.title = this.title.value;
      this.fitTitle();
      app.save();
    });

    this.editor = new CodeEditor($(".editor-pane .editor-host", this.root), {
      onChange: (text) => {
        if (this.tab.kind === "file") model.files[this.tab.name] = text;
        else model[this.tab.id] = text;
        this.changed();
      },
      onRun: () => this.run(),
      onFormat: () => this.format(),
      onCaretDiagnostics: (found) => this.showHint(found),
    });
    this.repl = new CodeEditor($(".repl-host", this.root), {
      label: "Console input",
      gutter: false,
      highlight: app.highlightSand,
      onKeyDown: (e) => this.replKey(e),
    });
    this.repl.textarea.wrap = "soft";

    for (const button of this.root.querySelectorAll("[data-action]")) {
      button.addEventListener("click", () => this[button.dataset.action]());
    }
    $(".terminal", this.root).addEventListener("click", () => {
      if (this.session && !getSelection()?.toString()) this.repl.focus();
    });
    this.show(this.tab);
    this.setSession(false);
  }

  fitTitle() {
    this.title.size = Math.max(6, Math.min(40, this.title.value.length + 1));
  }

  changed() {
    this.app.save();
    clearTimeout(this.checkTimer);
    this.checkTimer = setTimeout(() => this.check(), CHECK_DELAY);
  }

  fileNames() {
    const main = program(this.model);
    return Object.keys(this.model.files).sort((a, b) => (a === main ? -1 : b === main ? 1 : a.localeCompare(b)));
  }

  /**
   * Updates tab selection and rebuilds tab DOM elements only if file names or
   * error counts change, preserving double-click targets across clicks.
   */
  renderTabs() {
    const main = program(this.model);
    const errors = (name) => (this.diagnostics[name] ?? []).filter((d) => d.severity === "error").length;
    const signature = JSON.stringify([main, this.policyErrors.length, this.fileNames().map((n) => [n, errors(n)])]);
    if (signature !== this.tabSignature) {
      this.tabSignature = signature;
      this.buildTabs();
    }
    for (const tab of this.root.querySelectorAll(".pane-bar .tab")) {
      const active = tab.dataset.key === (this.tab.kind === "file" ? `file:${this.tab.name}` : `setting:${this.tab.id}`);
      tab.classList.toggle("active", active);
      tab.setAttribute("aria-selected", String(active));
    }
  }

  buildTabs() {
    const main = program(this.model);
    $(".setting-tabs", this.root).replaceChildren(
      ...SETTINGS.map((s) => {
        const errors = s.id === "policy" ? this.policyErrors.length : 0;
        const tab = this.tabElement(s.label, false, () => this.show({ kind: "setting", id: s.id }), { errors, title: s.hint });
        tab.dataset.key = `setting:${s.id}`;
        return tab;
      }),
    );
    const add = document.createElement("button");
    add.className = "add-file";
    add.textContent = "+";
    add.title = "New file";
    add.setAttribute("aria-label", "New file");
    add.addEventListener("click", () => this.newFile());
    $(".file-tabs", this.root).replaceChildren(
      ...this.fileNames().map((name) => {
        const errors = (this.diagnostics[name] ?? []).filter((d) => d.severity === "error").length;
        const where = name === main ? "The program" : `/${name} in the program's file system`;
        const tab = this.tabElement(name, false, () => this.show({ kind: "file", name }), {
          errors,
          title: `${where} (double-click to rename)`,
          mono: true,
          close: name === main ? null : () => this.removeFile(name),
        });
        tab.dataset.key = `file:${name}`;
        tab.querySelector(".tab-label").addEventListener("dblclick", () => this.rename(name, tab));
        return tab;
      }),
      add,
    );
  }

  tabElement(label, active, open, { errors = 0, title = "", mono = false, close = null }) {
    const tab = document.createElement("div");
    tab.className = "tab" + (active ? " active" : "") + (mono ? " mono" : "");
    tab.setAttribute("role", "tab");
    tab.setAttribute("aria-selected", String(active));
    const button = document.createElement("button");
    button.className = "tab-label";
    button.textContent = label;
    button.title = title;
    button.addEventListener("click", open);
    tab.append(button);
    if (errors) {
      const badge = document.createElement("span");
      badge.className = "badge error";
      badge.textContent = errors;
      badge.title = `${errors} error${errors === 1 ? "" : "s"}`;
      tab.append(badge);
    }
    if (close) {
      const x = document.createElement("button");
      x.className = "tab-close";
      x.textContent = "×";
      x.title = `Remove ${label}`;
      x.setAttribute("aria-label", `Remove ${label}`);
      x.addEventListener("click", close);
      tab.append(x);
    }
    return tab;
  }

  show(tab) {
    this.tab = tab;
    const editor = this.editor;
    if (tab.kind === "file") {
      editor.setHighlighter(tab.name.endsWith(".sand") ? this.app.highlightSand : null);
      editor.value = this.model.files[tab.name] ?? "";
      editor.textarea.placeholder = "";
      editor.textarea.setAttribute("aria-label", tab.name);
      editor.setDiagnostics(this.diagnostics[tab.name] ?? []);
    } else {
      const setting = SETTINGS.find((s) => s.id === tab.id);
      editor.setHighlighter(tab.id === "policy" ? highlightPolicy : null);
      editor.value = this.model[tab.id] ?? "";
      editor.textarea.placeholder = setting.placeholder;
      editor.textarea.setAttribute("aria-label", setting.label);
      editor.setDiagnostics(tab.id === "policy" ? this.policyDiagnostics() : []);
    }
    this.showHint([]);
    this.root.classList.toggle("on-setting", tab.kind === "setting");
    this.renderTabs();
  }

  showHint(found) {
    const hint = $(".tab-hint", this.root);
    hint.textContent = found.map((d) => `${d.severity}: ${d.message}`).join("  ·  ");
    hint.className = "tab-hint" + (found.length ? ` hint-${found.some((d) => d.severity === "error") ? "error" : "warning"}` : "");
    $(".pane-foot", this.root).hidden = !found.length;
  }

  newFile() {
    let name = "untitled.txt";
    for (let n = 2; name in this.model.files; n++) name = `untitled-${n}.txt`;
    this.model.files[name] = "";
    this.app.save();
    this.show({ kind: "file", name });
    const tab = [...this.root.querySelectorAll(".file-tabs .tab")].find((t) => t.querySelector(".tab-label").textContent === name);
    this.rename(name, tab);
  }

  rename(name, tab) {
    const label = tab.querySelector(".tab-label");
    const input = document.createElement("input");
    input.className = "tab-rename";
    input.value = name;
    input.spellcheck = false;
    input.setAttribute("aria-label", `New name of ${name}`);
    input.size = Math.max(8, name.length + 2);
    label.replaceWith(input);
    input.focus();
    const dot = name.lastIndexOf(".");
    input.setSelectionRange(name.lastIndexOf("/") + 1, dot > name.lastIndexOf("/") + 1 ? dot : name.length);
    let done = false;
    const finish = (commit) => {
      if (done) return;
      const to = input.value.trim().replace(/^\/+/, "");
      if (commit && to !== name) {
        const problem = !to || to.split("/").some((p) => p === "" || p === "." || p === "..") ? "A file name, such as notes.txt or lib/util.sand" : to in this.model.files ? `There is a file named ${to} already` : "";
        if (problem) {
          input.setCustomValidity(problem);
          input.reportValidity();
          return;
        }
      }
      done = true;
      if (commit && to !== name) this.renameFile(name, to);
      else {
        this.tabSignature = null;
        this.renderTabs();
      }
    };
    input.addEventListener("input", () => {
      input.setCustomValidity("");
      input.size = Math.max(8, input.value.length + 2);
    });
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        e.preventDefault();
        finish(true);
      } else if (e.key === "Escape") {
        finish(false);
      }
    });
    input.addEventListener("blur", () => finish(true));
  }

  renameFile(from, to) {
    const files = {};
    for (const [name, text] of Object.entries(this.model.files)) files[name === from ? to : name] = text;
    this.model.files = files;
    if (this.model.main === from) this.model.main = to;
    if (this.diagnostics[from]) {
      this.diagnostics[to] = this.diagnostics[from];
      delete this.diagnostics[from];
    }
    this.show(this.tab.kind === "file" && this.tab.name === from ? { kind: "file", name: to } : this.tab);
    this.changed();
  }

  removeFile(name) {
    const text = this.model.files[name];
    delete this.model.files[name];
    if (this.tab.kind === "file" && this.tab.name === name) this.tab = { kind: "file", name: program(this.model) };
    this.show(this.tab);
    this.changed();
    this.app.toast(`Removed ${name}`, () => {
      this.model.files[name] = text;
      this.show({ kind: "file", name });
      this.changed();
    });
  }

  policyDiagnostics() {
    const errors = this.policyErrors.map((e) => ({ line: e.line, column: e.column, width: 1000, severity: "error", message: e.message }));
    const warnings = this.policyWarnings.map((w) => ({ line: w.line, column: w.column, width: 1000, severity: "warning", message: `${w.message} [${w.code}]` }));
    return [...errors, ...warnings];
  }

  showDiagnostics(list) {
    const diagnostics = {};
    for (const d of list) (diagnostics[d.file] ??= []).push(d);
    this.diagnostics = diagnostics;
    if (this.tab.kind === "file") this.editor.setDiagnostics(diagnostics[this.tab.name] ?? []);
    else if (this.tab.id === "policy") this.editor.setDiagnostics(this.policyDiagnostics());
    this.renderTabs();
  }

  async check() {
    const generation = (this.checkGeneration = (this.checkGeneration ?? 0) + 1);
    let answer;
    try {
      answer = await this.app.tools.request({ type: "check", sandbox: this.snapshot() });
    } catch {
      this.app.restartTools();
      return;
    }
    if (generation !== this.checkGeneration) return;
    this.policyErrors = answer.policyErrors ?? [];
    this.policyWarnings = answer.policyWarnings ?? [];
    const found = answer.check?.diagnostics ?? [];
    const main = program(this.model);
    const lines = (this.model.files[main] ?? "").split("\n");
    const wordAt = (line, column) => Array.from(lines[line - 1] ?? "").slice(column - 1).join("").match(/^[\p{L}\p{N}_]+/u)?.[0].length ?? 1;
    const lint = (answer.lint ?? []).map((w) => ({ ...w, file: main, width: wordAt(w.line, w.column), severity: "warning", message: `${w.message} [${w.code}]` }));
    this.showDiagnostics([...found, ...lint]);
  }

  async lint() {
    let answer;
    try {
      answer = await this.app.tools.request({ type: "lint", sandbox: this.snapshot() });
    } catch {
      this.app.restartTools();
      return;
    }
    if (answer.error) {
      this.print(`Lint: the program must compile first.\n${answer.error}\n`, "error");
      return;
    }
    const n = answer.lint.length;
    if (!n) {
      this.print("Lint: 0 warnings.\n", "system");
      return;
    }
    const main = program(this.model);
    this.print(`Lint: ${n} warning${n === 1 ? "" : "s"}\n` + answer.lint.map((w) => `  ${main}:${w.line}:${w.column}  ${w.message} [${w.code}]\n`).join(""), "warning");
    this.show({ kind: "file", name: main });
  }

  async format() {
    const policy = this.tab.id === "policy";
    if (!policy && (this.tab.kind !== "file" || !this.tab.name.endsWith(".sand"))) return;
    let answer;
    try {
      answer = await this.app.tools.request({ type: "format", source: this.editor.value, policy });
    } catch {
      this.app.restartTools();
      return;
    }
    if (answer.error) this.app.toast(`Cannot format: ${answer.error.split("\n")[0]}`);
    else this.editor.replaceAll(answer.text);
  }

  snapshot() {
    return structuredClone(this.model);
  }

  sessionWorker() {
    this.worker ??= new EngineWorker(this.app.module, (event, reply) => this.event(event, reply));
    return this.worker;
  }

  event(event, reply) {
    if (event.type === "output") this.print(event.text, event.stream);
    else if (event.type === "prompt") this.ask(event.request, reply);
    else if (event.type === "ended") this.print(`[a task called exit(${event.exitCode}): the tasks stopped]\n`, "system");
  }

  async run() {
    if (this.running) return;
    clearTimeout(this.checkTimer);
    this.check();
    this.clearOutput();
    this.setSession(false);
    const run = this.startRunning("Running");
    const answer = await this.request({ type: "run", sandbox: this.snapshot() }, run);
    if (answer) this.showResult(answer.result, answer.written);
  }

  startRunning() {
    const run = (this.running = { stopped: false });
    this.root.classList.add("is-running");
    this.app.updateRunning();
    return run;
  }

  async request(message, run) {
    try {
      return await this.sessionWorker().request(message);
    } catch (e) {
      if (!run.stopped) {
        this.endSession();
        this.print(`internal error: ${e.message}\n`, "error");
      }
      return null;
    } finally {
      this.running = null;
      this.root.classList.remove("is-running");
      this.hidePrompt();
      this.app.updateRunning();
    }
  }

  stop() {
    if (!this.running) return;
    this.running.stopped = true;
    this.endSession();
    this.print("Stopped: the session ended.\n", "system");
  }

  endSession() {
    this.worker?.terminate();
    this.worker = null;
    this.setSession(false);
  }

  setSession(open) {
    this.session = open;
    this.repl.textarea.disabled = !open;
    this.repl.textarea.placeholder = open
      ? ""
      : "Run the program to start a session here.";
    $(".repl", this.root).hidden = !open;
    this.root.classList.toggle("has-session", open);
    this.updateOutput();
  }

  updateOutput() {
    const terminal = this.consoleEl.childElementCount > 0 || this.session;
    $(".terminal", this.root).hidden = !terminal;
    $(".output", this.root).hidden = !terminal && $(".prompt-bar", this.root).hidden && $(".written", this.root).hidden;
  }

  showResult(result, written) {
    const e = result.error;
    if (e) {
      this.print(errorText(e), "error");
      if (e.kind === "compile") this.showDiagnostics(e.diagnostics);
    } else {
      if (result.value !== undefined) this.print(result.value + "\n", "value");
      if (result.exitCode !== 0) this.print(`[exited with code ${result.exitCode}]\n`, "system");
    }
    this.setSession(e ? ["runtime", "limit", "interrupted"].includes(e.kind) : result.exitCode === 0);
    this.showWritten(written ?? []);
  }

  replKey(e) {
    if (e.key === "Enter" && !e.shiftKey && !e.altKey) {
      const source = this.repl.value;
      if (!source.trim()) return true;
      if (!this.app.isComplete(source) && !(e.metaKey || e.ctrlKey)) return false;
      this.submit(source);
      return true;
    }
    if ((e.key === "ArrowUp" || e.key === "ArrowDown") && !e.shiftKey && !e.altKey && !e.metaKey && !e.ctrlKey) {
      const up = e.key === "ArrowUp";
      if (!this.repl.caretOnEdgeLine(!up) || !this.history.length) return false;
      this.historyAt = Math.max(0, Math.min(this.history.length, (this.historyAt ?? this.history.length) + (up ? -1 : 1)));
      this.repl.value = this.history[this.historyAt] ?? "";
      this.repl.textarea.setSelectionRange(this.repl.value.length, this.repl.value.length);
      return true;
    }
    return false;
  }

  async submit(source) {
    if (this.running || !this.session) return;
    this.history = [...this.history.filter((h) => h !== source), source].slice(-MAX_HISTORY);
    this.historyAt = undefined;
    this.repl.value = "";
    this.print(source.split("\n").map((l, i) => (i ? "  " : "› ") + l).join("\n") + "\n", "input");
    const run = this.startRunning();
    const answer = await this.request({ type: "input", source }, run);
    if (!answer) return;
    const r = answer.result;
    if (r.error) this.print(errorText(r.error), "error");
    else if (r.value !== undefined) this.print(r.value + "\n", "value");
    if (r.exitCode !== undefined) this.print(`[exited with code ${r.exitCode}]\n`, "system");
    this.showWritten(answer.written ?? []);
    this.repl.focus();
  }

  showWritten(files) {
    const list = $(".written", this.root);
    list.replaceChildren();
    list.hidden = files.length === 0;
    this.updateOutput();
    if (!files.length) return;
    const heading = document.createElement("span");
    heading.textContent = "Files written:";
    list.append(heading);
    for (const { path, data } of files) {
      const button = document.createElement("button");
      button.className = "chip";
      button.textContent = `${path} · ${bytes(data.byteLength)}`;
      button.title = "Open in a tab";
      button.addEventListener("click", () => {
        const name = path.replace(/^\/+/, "");
        this.model.files[name] = new TextDecoder().decode(data);
        this.app.save();
        this.show({ kind: "file", name });
      });
      list.append(button);
    }
  }

  clearOutput() {
    this.consoleEl.replaceChildren();
    this.consoleSize = 0;
    this.showWritten([]);
  }

  print(text, kind = "stdout") {
    const terminal = $(".terminal", this.root);
    const atBottom = terminal.hidden || terminal.scrollHeight - terminal.scrollTop - terminal.clientHeight < 24;
    const last = this.consoleEl.lastElementChild;
    if (last && last.dataset.kind === kind && kind !== "input") last.append(text);
    else {
      const span = document.createElement("span");
      span.className = `out-${kind}`;
      span.dataset.kind = kind;
      if (kind === "input") span.innerHTML = this.highlightInput(text);
      else span.textContent = text;
      this.consoleEl.append(span);
    }
    this.consoleSize = (this.consoleSize ?? 0) + text.length;
    while (this.consoleSize > MAX_CONSOLE && this.consoleEl.childElementCount > 1) {
      const first = this.consoleEl.firstElementChild;
      this.consoleSize -= first.textContent.length;
      first.remove();
      if (!this.consoleEl.firstElementChild?.classList.contains("out-truncated")) {
        const note = document.createElement("span");
        note.className = "out-truncated";
        note.textContent = "… earlier output removed\n";
        this.consoleEl.prepend(note);
      }
    }
    this.updateOutput();
    if (atBottom || kind === "input") terminal.scrollTop = terminal.scrollHeight;
  }

  highlightInput(text) {
    const escape = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);
    return text
      .split("\n")
      .map((line) => {
        const code = line.slice(2);
        let html = `<span class="prompt-mark">${escape(line.slice(0, 2))}</span>`;
        let at = 0;
        for (const [start, end, cls] of this.app.highlightSand(code)) {
          html += escape(code.slice(at, start)) + `<span class="${cls}">${escape(code.slice(start, end))}</span>`;
          at = end;
        }
        return html + escape(code.slice(at));
      })
      .join("\n");
  }

  ask(request, reply) {
    const bar = $(".prompt-bar", this.root);
    $(".prompt-text", bar).textContent = `The program asks to ${request.description}.`;
    bar.hidden = false;
    this.updateOutput();
    const rules = { always: request.allowRule, never: request.denyRule };
    const answer = (value) => {
      bar.hidden = true;
      this.updateOutput();
      const allowed = value === "once" || value === "always";
      const rule = rules[value];
      if (rule) this.addRule(rule);
      this.print(`[${allowed ? "allowed" : "denied"}: ${request.description}${rule ? `; added to the policy: ${rule}` : ""}]\n`, "system");
      reply(value);
    };
    for (const button of bar.querySelectorAll("[data-answer]")) {
      const rule = rules[button.dataset.answer];
      button.title = rule ? `Add “${rule}” to the policy` : button.dataset.answer === "once" ? "Allow this time" : "Deny this time";
      button.onclick = () => answer(button.dataset.answer);
    }
    bar.querySelector("[data-answer=once]").focus();
  }

  addRule(rule) {
    const policy = this.model.policy ?? "";
    if (policy.split("\n").some((line) => line.split("#")[0].trim() === rule)) return;
    this.model.policy = policy + (policy === "" || policy.endsWith("\n") ? "" : "\n") + rule + "\n";
    if (this.tab.kind === "setting" && this.tab.id === "policy") this.editor.value = this.model.policy;
    this.changed();
  }

  hidePrompt() {
    $(".prompt-bar", this.root).hidden = true;
    this.updateOutput();
  }

  remove() {
    this.app.remove(this);
  }

  destroy() {
    this.worker?.terminate();
    this.root.remove();
  }
}

function errorText(e) {
  let text = e.message.endsWith("\n") ? e.message : e.message + "\n";
  if (e.kind === "runtime") for (const t of e.trace) text += `    at ${t.function} (${t.file}:${t.line})\n`;
  return text;
}

class Playground {
  constructor(wasm, module) {
    this.module = module;
    this.views = [];
    this.tools = new EngineWorker(module);
    const kinds = wasm.highlightKinds();
    this.highlightSand = (text) => {
      const flat = wasm.highlight(text);
      const ranges = [];
      for (let i = 0; i < flat.length; i += 3) ranges.push([flat[i], flat[i + 1], kinds[flat[i + 2]]]);
      return ranges;
    };
    this.isComplete = (source) => wasm.Session.isComplete(source);
    $("#new-sandbox").addEventListener("click", () => this.add(newSandbox({ title: `Sandbox ${this.views.length + 1}`, policy: "limit time 5s\n" })).editor.focus());
    $("#share").addEventListener("click", () => this.share());
    this.examplesMenu();
  }

  examplesMenu() {
    const button = $("#examples-button");
    const menu = $("#examples-menu");
    for (const example of examples) {
      const item = document.createElement("button");
      item.type = "button";
      item.setAttribute("role", "menuitem");
      item.textContent = example.title;
      item.addEventListener("click", () => {
        menu.hidePopover();
        this.add(fromExample(example)).root.scrollIntoView({ behavior: "smooth", block: "start" });
      });
      menu.append(item);
    }
    const items = () => [...menu.querySelectorAll("[role=menuitem]")];
    const place = () => {
      const r = button.getBoundingClientRect();
      menu.style.minWidth = `${r.width}px`;
      menu.style.top = `${r.bottom + 4}px`;
      menu.style.left = `${Math.max(8, Math.min(r.left, innerWidth - menu.offsetWidth - 8))}px`;
    };
    menu.addEventListener("toggle", (e) => {
      button.setAttribute("aria-expanded", String(e.newState === "open"));
      if (e.newState === "open") {
        place();
        items()[0]?.focus();
      }
    });
    addEventListener("resize", () => menu.matches(":popover-open") && place());
    addEventListener("scroll", () => menu.matches(":popover-open") && menu.hidePopover(), { passive: true });
    menu.addEventListener("keydown", (e) => {
      const list = items();
      const at = list.indexOf(document.activeElement);
      const to = { ArrowDown: at + 1, ArrowUp: at - 1, Home: 0, End: list.length - 1 }[e.key];
      if (to === undefined) return;
      e.preventDefault();
      list[(to + list.length) % list.length].focus();
    });
  }

  restartTools() {
    this.tools.terminate();
    this.tools = new EngineWorker(this.module);
  }

  load(models) {
    for (const v of this.views) v.destroy();
    this.views = [];
    for (const m of models) this.add(m);
  }

  add(model, after = null) {
    const view = new SandboxView(this, model);
    if (after) {
      after.root.after(view.root);
      this.views.splice(this.views.indexOf(after) + 1, 0, view);
    } else {
      $("#sandboxes").append(view.root);
      this.views.push(view);
    }
    view.check();
    this.save();
    return view;
  }

  remove(view) {
    const index = this.views.indexOf(view);
    const model = view.model;
    this.views.splice(index, 1);
    view.destroy();
    this.updateRunning();
    this.save();
    this.toast(`Deleted “${model.title}”`, () => {
      const restored = new SandboxView(this, model);
      const next = this.views[index];
      if (next) next.root.before(restored.root);
      else $("#sandboxes").append(restored.root);
      this.views.splice(index, 0, restored);
      restored.check();
      this.save();
    });
  }

  save() {
    clearTimeout(this.saveTimer);
    this.saveTimer = setTimeout(() => {
      try {
        localStorage.setItem(STORAGE_KEY, JSON.stringify(this.views.map((v) => v.model)));
      } catch {
        // Ignore storage quota or access errors.
      }
    }, 300);
  }

  updateRunning() {
    const n = this.views.filter((v) => v.running).length;
    $("#running").textContent = n ? `${n} running` : "";
  }

  async share() {
    const code = await encodeShare(this.views.map((v) => v.model));
    const url = `${location.origin}${location.pathname}#share=${code}`;
    history.replaceState(null, "", `#share=${code}`);
    try {
      await navigator.clipboard.writeText(url);
      this.toast("Link copied: it opens these sandboxes");
    } catch {
      this.toast("The address bar now holds a link to these sandboxes");
    }
  }

  toast(message, undo) {
    const toast = $("#toast");
    $(".toast-text", toast).textContent = message;
    const button = $("button", toast);
    button.hidden = !undo;
    button.onclick = () => {
      toast.hidden = true;
      undo?.();
    };
    toast.hidden = false;
    clearTimeout(this.toastTimer);
    this.toastTimer = setTimeout(() => (toast.hidden = true), 6000);
  }
}

function fromExample(example) {
  return newSandbox({ title: example.title, files: example.files, policy: example.policy ?? "", env: example.env ?? "", stdin: example.stdin ?? "" });
}

function seconds(ms) {
  return `${(ms / 1000).toFixed(1)} s`;
}

function millis(ms) {
  return ms < 1000 ? `${ms.toFixed(ms < 10 ? 1 : 0)} ms` : seconds(ms);
}

function bytes(n) {
  return n < 1024 ? `${n} B` : n < 1 << 20 ? `${(n / 1024).toFixed(1)} KB` : `${(n / (1 << 20)).toFixed(1)} MB`;
}

async function initialSandboxes() {
  let saved = [];
  try {
    const list = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null");
    if (Array.isArray(list)) saved = list.map((s) => newSandbox(s));
  } catch {
    // Ignore corrupted localStorage state.
  }
  const share = new URLSearchParams(location.hash.slice(1)).get("share");
  if (share) {
    history.replaceState(null, "", location.pathname + location.search);
    try {
      const shared = await decodeShare(share);
      const key = (s) => JSON.stringify([s.title, s.main, s.files, s.policy, s.env, s.stdin]);
      const keys = new Set(shared.map(key));
      return [...shared, ...saved.filter((s) => !keys.has(key(s)))];
    } catch (e) {
      console.warn("invalid share link", e);
    }
  }
  if (saved.length) return saved;
  return ["welcome", "capabilities"].map((id) => fromExample(examples.find((e) => e.id === id)));
}

async function main() {
  const started = performance.now();
  const status = $("#engine-status");
  try {
    const wasm = await import("./pkg/sandlang_wasm.js");
    const module = await compileModule(new URL("./pkg/sandlang_wasm_bg.wasm", import.meta.url));
    await wasm.default({ module_or_path: module });
    const playground = new Playground(wasm, module);
    playground.load(await initialSandboxes());
    status.textContent = `Sand ${wasm.version()} · loaded in ${millis(performance.now() - started)}`;
    document.body.classList.add("ready");
  } catch (e) {
    status.textContent = `The engine failed to load: ${e.message}. Build it with the site: cargo site serve`;
    status.classList.add("error");
    throw e;
  }
}

main();
