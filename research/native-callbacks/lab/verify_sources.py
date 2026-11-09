"""Downloads each cited primary source and checks that the quoted text is in it.

Usage: python3 -I verify_sources.py > results/sources.txt
"""
import re, sys, urllib.request

RAW = "https://raw.githubusercontent.com"
CHECKS = [
    # Lua
    ("L-S1", f"{RAW}/lua/lua/v5.4.6/manual/manual.of", "Internally, Lua uses the C @id{longjmp} facility to handle errors"),
    ("L-S2", f"{RAW}/lua/lua/v5.4.6/manual/manual.of", "and then calls @T{abort}"),
    ("L-S3", f"{RAW}/lua/lua/v5.4.6/llimits.h", "#define LUAI_MAXCCALLS\t\t200"),
    ("L-S3b", f"{RAW}/lua/lua/v5.5.0/ldo.h", "LUAI_MAXCCALLS"),
    ("L-S4", f"{RAW}/lua/lua/v5.5.0/manual/manual.of", "lua_setcstacklimit"),
    ("L-S5", f"{RAW}/lua/lua/v5.4.6/ldo.c", "attempt to yield across a C-call boundary"),
    ("L-S5b", f"{RAW}/lua/lua/v5.4.6/manual/manual.of", "was destroyed by the yield"),
    ("L-S6", f"{RAW}/lua/lua/v5.4.6/manual/manual.of", "This event only happens while Lua is executing a Lua function"),
    ("L-S9", f"{RAW}/luau-lang/site/HEAD/src/content/docs/guides/sandbox.md", "load trusted and untrusted code into separate VMs"),
    ("L-S10", f"{RAW}/luau-lang/luau/master/VM/include/lua.h", "lua_pushcclosurek"),
    ("L-S10b", f"{RAW}/luau-lang/luau/master/VM/include/luaconf.h", "LUAI_MAXCCALLS"),
    ("L-S11", f"{RAW}/Roblox/roact/master/src/SingleEventManager.lua", "event:Connect(function(...)"),
    ("L-S12", f"{RAW}/LuaJIT/LuaJIT/v2.1/doc/ext_ffi_semantics.html", "bad callback"),
    ("L-S13", f"{RAW}/mlua-rs/mlua/v0.12.2/README.md", "is protected by `lua_pcall`"),
    ("L-S13b", f"{RAW}/mlua-rs/mlua/v0.12.2/src/error.rs", "called the same mutable callback again"),
    # CPython
    ("P-S1", f"{RAW}/python/cpython/v3.13.0/Doc/extending/extending.rst", "terminated by raising an exception"),
    ("P-S3", f"{RAW}/python/cpython/v3.12.0/Doc/whatsnew/3.12.rst", "The recursion limit now applies only to Python code"),
    ("P-S3b", f"{RAW}/python/cpython/v3.13.0/Include/cpython/pystate.h", "Py_C_RECURSION_LIMIT 10000"),
    ("P-S4", f"{RAW}/python/cpython/v3.14.0/Python/ceval.c", "Stack overflow (used %d kB)%s"),
    ("P-S5", f"{RAW}/python/cpython/v3.11.0/Doc/whatsnew/3.11.rst", "Most Python function calls now consume no C stack space"),
    ("P-S6", f"{RAW}/python/peps/main/peps/pep-0219.rst", "the C stack now holds a nested execution of the byte-code interpreter"),
    ("P-S7", f"{RAW}/python-greenlet/greenlet/master/docs/caveats.rst", "native (C) frames"),
    ("P-S8", f"{RAW}/python/cpython/v3.13.0/Objects/listobject.c", "allowing mutations during sorting is a core-dump"),
    ("P-S9", f"{RAW}/python/cpython/v3.13.0/Doc/extending/extending.rst", "implicit invocations of the interpreter"),
    ("P-S10", f"{RAW}/python/peps/main/peps/pep-0578.rst", "This is not sandboxing"),
    ("P-S11", f"{RAW}/python/peps/main/peps/pep-0684.rst", "file descriptors, environment variables) are shared"),
    ("P-S13", f"{RAW}/PyO3/pyo3/main/src/pycell.rs", "Already borrowed"),
    # WebAssembly
    ("W-S7", f"{RAW}/WebAssembly/component-model/ef29236d08/design/mvp/CanonicalABI.md", "cannot be externally reentered"),
    ("W-S8", f"{RAW}/WebAssembly/component-model/main/design/mvp/Concurrency.md", "may **reenter** the caller's component instance"),
    ("W-S10", f"{RAW}/WebAssembly/js-promise-integration/main/proposals/js-promise-integration/Overview.md", "reentrant"),
    ("W-S12", f"{RAW}/wasm3/wasm3/main/README.md", "you can't generate executable code pages in runtime"),
    # JS and UI frameworks
    ("J-S1", f"{RAW}/facebook/react-native-website/main/website/blog/2024-10-23-the-new-architecture-is-here.mdx", "Since the old architecture was only asynchronous"),
    ("J-S1b", f"{RAW}/facebook/react-native-website/main/website/blog/2024-10-23-the-new-architecture-is-here.mdx", "layout could not be read synchronously"),
    ("J-S2", f"{RAW}/facebook/hermes/main/API/jsi/jsi/jsi.h", "The APIs must not be called from multiple threads concurrently"),
    ("J-S3", f"{RAW}/facebook/react-native/main/packages/react-native/ReactCommon/callinvoker/ReactCommon/CallInvoker.h", "invokeSync"),
    ("J-S4", f"{RAW}/bellard/quickjs/master/doc/quickjs.texi", "no multi-threading is supported"),
    ("J-S6", f"{RAW}/flutter/website/main/sites/docs/src/content/platform-integration/platform-channels.md", "passed asynchronously through channels"),
    ("J-S7", f"{RAW}/dart-lang/sdk/main/sdk/lib/ffi/ffi.dart", "It will abort the process if invoked on any"),
    ("J-S7b", f"{RAW}/dart-lang/sdk/main/sdk/lib/ffi/ffi.dart", "The native code does not wait for a response from the callback, so only"),
    ("J-S9", f"{RAW}/NativeScript/docs/main/content/guide/multithreading.md", "JavaScript executes on the main thread"),
    ("J-S11", f"{RAW}/androidx/androidx/androidx-main/compose/ui/ui/src/commonMain/kotlin/androidx/compose/ui/layout/MeasurePolicy.kt", "fun MeasureScope.measure("),
    ("J-S11b", f"{RAW}/androidx/androidx/androidx-main/compose/runtime/runtime/src/commonMain/kotlin/androidx/compose/runtime/Effects.kt", "block: suspend CoroutineScope.() -> Unit"),
    ("J-S12", f"{RAW}/lynx-family/lynx-website/main/docs/en/react/main-thread-script.mdx", "lagging behind gestures"),
    ("J-S13", f"{RAW}/Snapchat/Valdi/main/docs/docs/native-bindings.md", "automatically asynchronously dispatch to the JS thread"),
    # Rust-native interpreters, Starlark, BEAM, Wren
    ("R-S1", f"{RAW}/kyren/piccolo/master/README.md", "It does not rely on the rust stack"),
    ("R-S1b", f"{RAW}/kyren/piccolo/master/README.md", "much more difficult than writing in normal"),
    ("R-S5", f"{RAW}/google/starlark-go/master/starlark/interp.go", "called recursively"),
    ("R-S5b", f"{RAW}/bazelbuild/starlark/master/spec.md", "all values in its top-level environment are frozen"),
    ("R-S7", f"{RAW}/erlang/otp/OTP-28.1/erts/doc/references/erl_nif.md", "within 1 millisecond"),
    ("R-S7b", f"{RAW}/erlang/otp/OTP-28.1/erts/doc/references/erl_nif.md", "This approach is always preferred over the other alternatives"),
    ("R-S8", f"{RAW}/wren-lang/wren/0.4.0/doc/site/embedding/calling-c-from-wren.markdown", "The VM is not re-entrant"),
    ("R-S8b", f"{RAW}/wren-lang/wren/0.4.0/src/vm/wren_vm.c", "Cannot already be in foreign call."),
]

def norm(s):
    return re.sub(r"\s+", " ", s)

cache = {}
ok = 0
for cid, url, needle in CHECKS:
    try:
        if url not in cache:
            with urllib.request.urlopen(url, timeout=30) as r:
                cache[url] = norm(r.read().decode("utf-8", "replace"))
        found = norm(needle) in cache[url]
    except Exception as e:
        print(f"{cid} FETCH-FAILED {url} ({e})")
        continue
    ok += found
    print(f"{cid} {'FOUND' if found else 'MISSING'} {url} :: {needle!r}")
print(f"{ok}/{len(CHECKS)} quotes found")

# Facts quoted from crate sources, read from the local cargo registry
# (the same files docs.rs renders). Run after `cargo fetch` in this directory.
import glob, os
REG = glob.glob(os.path.expanduser("~/.cargo/registry/src/*/"))[0]
LOCAL = [
    ("W-S2", "wasmtime-50.0.1/src/runtime/store.rs", "cannot do blocking `call` from an async context"),
    ("W-S4", "wasmtime-50.0.1/src/lib.rs", "separately allocated native stack"),
    ("W-S5", "wasmtime-internal-fiber-50.0.1/src/stackswitch.rs", "No support for this platform"),
    ("W-S5b", "wasmtime-internal-fiber-50.0.1/src/nostd.rs", "fibers unsupported on this host architecture"),
    ("W-S6", "wasmtime-50.0.1/src/config.rs", "space consumed by a host function is counted towards this limit"),
    ("W-S8b", "wasmtime-environ-50.0.1/src/trap_encoding.rs", "cannot enter component instance"),
    ("W-S8c", "wasmtime-50.0.1/src/runtime/component/store.rs", "We return `false` here only `self` has been poisoned due to a trap."),
    ("W-S9", "wasmi-2.0.0/src/func/into_func.rs", "runs on another `Stack`"),
    ("W-S9b", "wasmi-2.0.0/src/func/mod.rs", "This is a non-standard WebAssembly API"),
    ("R-S2", "piccolo-0.3.3/src/thread/executor.rs", "will panic"),
    ("R-S3", "rhai-1.27.0/src/api/limits.rs", "pub const MAX_CALL_STACK_DEPTH: usize = 64;"),
    ("R-S4", "rune-0.14.2/src/runtime/function.rs", "let mut vm = Vm::new(self.context.clone(), self.unit.clone());"),
    ("R-S4b", "rune-0.14.2/src/runtime/budget.rs", "without explicit co-operation from native"),
    ("R-S6", "starlark-0.14.2/src/eval/runtime/evaluator.rs", "DEFAULT_STACK_SIZE: usize = 50;"),
]
ok = 0
for cid, path, needle in LOCAL:
    try:
        found = norm(needle) in norm(open(REG + path, encoding="utf-8").read())
    except OSError as e:
        print(f"{cid} READ-FAILED {path} ({e})"); continue
    ok += found
    print(f"{cid} {'FOUND' if found else 'MISSING'} crates.io:{path} :: {needle!r}")
print(f"{ok}/{len(LOCAL)} crate-source quotes found")
