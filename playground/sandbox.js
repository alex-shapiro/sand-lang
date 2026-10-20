// What a sandbox is, without the page: its files, policy, environment and
// input, and how they become the options of an engine (see wasm/src/lib.rs).
// Shared by the page, its workers and the tests.

/** The default name of the program file. */
export const MAIN = "main.sand";

/**
 * A sandbox: `files` maps names (`main.sand`, `data/x.txt`, ...) to text,
 * `main` names the program among them (the others go to the program's file
 * system); `policy`, `env` (`NAME=value` lines) and `stdin` are text.
 */
export function newSandbox(fields = {}) {
  const sandbox = { title: "Sandbox", main: MAIN, files: { [MAIN]: "" }, policy: "", env: "", stdin: "", ...structuredClone(fields) };
  if (!(sandbox.main in sandbox.files)) sandbox.files[sandbox.main] = "";
  return sandbox;
}

/** The name of a sandbox's program file. */
export function program(sandbox) {
  return sandbox.main ?? MAIN;
}

/** The absolute path of a file of a sandbox in the file system. */
export function filePath(name) {
  return "/" + name.replace(/^\/+/, "");
}

/** `NAME=value` lines (blank lines and `#` comments ignored) as an object. */
export function parseEnv(text) {
  const env = {};
  for (const raw of (text ?? "").split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq > 0) env[line.slice(0, eq).trim()] = line.slice(eq + 1);
  }
  return env;
}

/** The options of the engine that runs `sandbox`. */
export function engineOptions(sandbox) {
  const files = {};
  for (const [name, text] of Object.entries(sandbox.files ?? {})) {
    if (name !== program(sandbox)) files[filePath(name)] = text;
  }
  const options = { policy: sandbox.policy ?? "", files, env: parseEnv(sandbox.env) };
  if (sandbox.stdin) options.stdin = sandbox.stdin;
  return options;
}

/** Encodes sandboxes for a link (deflated JSON, base64url). */
export async function encodeShare(sandboxes) {
  const json = JSON.stringify(sandboxes.map(({ title, main, files, policy, env, stdin }) => ({ title, main, files, policy, env, stdin })));
  const stream = new Blob([json]).stream().pipeThrough(new CompressionStream("deflate-raw"));
  const bytes = new Uint8Array(await new Response(stream).arrayBuffer());
  let binary = "";
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

/** Decodes `encodeShare`'s text; throws if it is not valid. */
export async function decodeShare(text) {
  const binary = atob(text.replaceAll("-", "+").replaceAll("_", "/"));
  const bytes = Uint8Array.from(binary, (c) => c.charCodeAt(0));
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
  const list = JSON.parse(await new Response(stream).text());
  if (!Array.isArray(list)) throw new Error("not a list of sandboxes");
  return list.map((s) => newSandbox({
    title: String(s.title ?? "Sandbox"),
    main: String(s.main ?? MAIN),
    files: Object.fromEntries(Object.entries(s.files ?? { [MAIN]: "" }).map(([k, v]) => [String(k), String(v)])),
    policy: String(s.policy ?? ""),
    env: String(s.env ?? ""),
    stdin: String(s.stdin ?? ""),
  }));
}
