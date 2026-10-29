// Node repro: PKG=/Users/alex/Projects/sandlang/target/wasm-node node repro.mjs
// Build the pkg first with: wasm/build.sh nodejs target/wasm-node
import { createRequire } from "node:module";
const sand = createRequire(import.meta.url)(process.env.PKG + "/sandlang_wasm.js");
const engine = new sand.Engine({ policy: "allow stdout\nlimit time 3s\n" }, { onOutput: (_, t) => process.stdout.write(t) });
const start = Date.now();
setTimeout(() => { console.log(`interrupt() called at ${Date.now() - start} ms`); engine.interrupt(); }, 100);
const r = await engine.run("var i = 0\nwhile true { i += 1 }");
console.log(`run ended at ${Date.now() - start} ms:`, r.error.kind, "-", r.error.message);
