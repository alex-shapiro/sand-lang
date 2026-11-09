# Checked quotes

Each quote or code fragment that `report.md` cites by source ID, with its
source. `lab/verify_sources.py` holds the same list and checks every entry;
regenerate this file from it after a change. `crates:` paths are in the
crates.io source of that version, which docs.rs also shows. `repo:` paths
are in this repository. The other `sources/*.md` files hold research notes.

- **J-S1** https://raw.githubusercontent.com/facebook/react-native-website/266ef4d753fdf274efb8aafb575762c01032ead6/website/blog/2024-10-23-the-new-architecture-is-here.mdx
  > some updates need to render synchronously in response to user input
- **J-S10** https://developer.apple.com/tutorials/data/documentation/swiftui/view.json
  > @MainActor @preconcurrency protocol View
- **J-S10b** https://developer.apple.com/tutorials/data/documentation/swiftui/layout.json
  > func sizeThatFits(proposal: ProposedViewSize, subviews: Self.Subviews, cache: inout Self.Cache) -> CGSize
- **J-S11** https://developer.android.com/develop/ui/compose/mental-model
  > as often as every frame of an animation
- **J-S11b** https://raw.githubusercontent.com/androidx/androidx/edf01d0d436697bc1a6b3bffdf65a462b756f88f/compose/ui/ui/src/commonMain/kotlin/androidx/compose/ui/layout/MeasurePolicy.kt
  > fun MeasureScope.measure(
- **J-S11c** https://raw.githubusercontent.com/androidx/androidx/edf01d0d436697bc1a6b3bffdf65a462b756f88f/compose/runtime/runtime/src/commonMain/kotlin/androidx/compose/runtime/Effects.kt
  > block: suspend CoroutineScope.() -> Unit
- **J-S12** https://raw.githubusercontent.com/lynx-family/lynx-website/e6a8e427758ccead03ef7648065478dff43a9a2a/docs/en/react/main-thread-script.mdx
  > animations lagging behind gestures
- **J-S13** https://raw.githubusercontent.com/Snapchat/Valdi/b527b091ea98b4952b1a525388d0c0b66ca04940/docs/docs/native-bindings.md
  > calling a JS function will automatically asynchronously dispatch to the JS thread if needed
- **J-S13b** https://raw.githubusercontent.com/Snapchat/Valdi/b527b091ea98b4952b1a525388d0c0b66ca04940/docs/docs/native-bindings.md
  > If your function need to return a value synchronously to platform code, make sure you call this function within a createScopedJSRuntime scope
- **J-S1b** https://raw.githubusercontent.com/facebook/react-native-website/266ef4d753fdf274efb8aafb575762c01032ead6/website/blog/2024-10-23-the-new-architecture-is-here.mdx
  > Since the old architecture was only asynchronous, we needed to rewrite it
- **J-S1c** https://raw.githubusercontent.com/facebook/react-native-website/266ef4d753fdf274efb8aafb575762c01032ead6/website/blog/2024-10-23-the-new-architecture-is-here.mdx
  > layout could not be read synchronously
- **J-S2** https://raw.githubusercontent.com/facebook/hermes/07c4e3488d90cc8dddbdd360ad4a93b2fb0d912c/API/jsi/jsi/jsi.h
  > The APIs must not be called from multiple threads concurrently
- **J-S3** https://raw.githubusercontent.com/facebook/react-native/22519b0e0c156ba9b53f50a06aa170131d757c69/packages/react-native/ReactCommon/callinvoker/ReactCommon/CallInvoker.h
  > virtual void invokeSync(CallFunc &&func)
- **J-S4** https://raw.githubusercontent.com/bellard/quickjs/e5ac135f8db58f8d6b7b5862b8263ce6ba9c5ab6/doc/quickjs.texi
  > Inside a given runtime, no multi-threading is supported
- **J-S5** https://developer.apple.com/tutorials/data/documentation/javascriptcore/jsvirtualmachine.json
  > other threads attempting to use the same virtual machine must wait
- **J-S6** https://raw.githubusercontent.com/flutter/website/b3ec0c2f546294a91d6ad14c4f30c5a80a82d712/sites/docs/src/content/platform-integration/platform-channels.md
  > asynchronously through channels to ensure the user interface remains responsive
- **J-S7** https://raw.githubusercontent.com/dart-lang/sdk/55afb51719372c2b2a9799852bddb46dc096fee8/sdk/lib/ffi/ffi.dart
  > It will abort the process if invoked on any other thread
- **J-S7b** https://raw.githubusercontent.com/dart-lang/sdk/55afb51719372c2b2a9799852bddb46dc096fee8/sdk/lib/ffi/ffi.dart
  > only functions returning void are supported
- **J-S8** https://raw.githubusercontent.com/flutter/website/b3ec0c2f546294a91d6ad14c4f30c5a80a82d712/sites/docs/src/content/release/breaking-changes/macos-windows-merged-threads.md
  > The split-thread design prevented Flutter apps and plugins from using Dart FFI
- **J-S9** https://raw.githubusercontent.com/NativeScript/docs/fe3d25290163f7802b7487863c4a36c5694e3ca9/content/guide/multithreading.md
  > JavaScript executes on the main thread
- **L-S1** https://raw.githubusercontent.com/lua/lua/v5.4.8/manual/manual.of
  > Lua uses the C longjmp facility to handle errors
- **L-S10** https://raw.githubusercontent.com/luau-lang/luau/12a6ba75883272c79b66b59815efef4c574196f6/VM/include/lua.h
  > lua_pushcclosurek
- **L-S11** https://raw.githubusercontent.com/Roblox/roact/1676d95c4886d51ee2b21bfcd55c6e50ece799e5/src/SingleEventManager.lua
  > event:Connect(function(...)
- **L-S13** https://raw.githubusercontent.com/mlua-rs/mlua/v0.11.6/README.md
  > is protected by `lua_pcall`
- **L-S13b** https://raw.githubusercontent.com/mlua-rs/mlua/v0.11.6/src/error.rs
  > called the same mutable callback again
- **L-S14** https://raw.githubusercontent.com/lua/lua/v5.4.8/ltablib.c
  > Quicksort algorithm (recursive function)
- **L-S3** https://raw.githubusercontent.com/lua/lua/v5.4.8/llimits.h
  > #define LUAI_MAXCCALLS 200
- **L-S3b** https://raw.githubusercontent.com/lua/lua/v5.4.8/lstate.c
  > luaG_runerror(L, "C stack overflow")
- **L-S5** https://raw.githubusercontent.com/lua/lua/v5.4.8/manual/manual.of
  > its frame in the C stack was destroyed by the yield
- **L-S5b** https://raw.githubusercontent.com/lua/lua/v5.4.8/manual/manual.of
  > Lua calls a continuation function
- **L-S5c** https://raw.githubusercontent.com/lua/lua/v5.4.8/ldo.c
  > attempt to yield across a C-call boundary
- **L-S6** https://raw.githubusercontent.com/lua/lua/v5.4.8/manual/manual.of
  > This event only happens while Lua is executing a Lua function
- **L-S8** https://raw.githubusercontent.com/lua/lua/v5.4.8/manual/manual.of
  > Creates and returns a reference
- **P-S10** https://raw.githubusercontent.com/python/peps/efec2601611c2fa53b1b1791b09b0368840c038c/peps/pep-0578.rst
  > Sandboxing CPython has been attempted many times in the past, and each past attempt has failed
- **P-S13** https://raw.githubusercontent.com/PyO3/pyo3/f41d7e8799cad7f1c393d00276a4fa7b83acf885/src/pycell.rs
  > Already borrowed
- **P-S14** https://raw.githubusercontent.com/python/cpython/v3.13.0/Objects/listsort.txt
  > an adaptive, stable, natural mergesort
- **P-S3b** https://raw.githubusercontent.com/python/cpython/v3.13.0/Include/cpython/pystate.h
  > Py_C_RECURSION_LIMIT 10000
- **P-S4** https://raw.githubusercontent.com/python/cpython/v3.14.0/Python/ceval.c
  > Stack overflow (used %d kB)%s
- **P-S6** https://raw.githubusercontent.com/python/peps/efec2601611c2fa53b1b1791b09b0368840c038c/peps/pep-0219.rst
  > the C stack now holds a nested execution of the byte-code interpreter
- **P-S7** https://raw.githubusercontent.com/python-greenlet/greenlet/4c22fe41c284a5c5b0ecd19f4607121cd8b31847/docs/caveats.rst
  > native (C) frames
- **P-S8** https://raw.githubusercontent.com/python/cpython/v3.13.0/Objects/listobject.c
  > allowing mutations during sorting is a core-dump factory
- **R-S1** https://raw.githubusercontent.com/kyren/piccolo/ce709eb1dae5c543cbc78e7e12bb80249d88c55f/README.md
  > It does not rely on the rust stack
- **R-S1b** https://raw.githubusercontent.com/kyren/piccolo/ce709eb1dae5c543cbc78e7e12bb80249d88c55f/README.md
  > writing things as a `Sequence` implementation is much more difficult than writing in normal, straight control flow
- **R-S1c** https://raw.githubusercontent.com/kyren/piccolo/ce709eb1dae5c543cbc78e7e12bb80249d88c55f/README.md
  > there are a few sources of unsafety, but crucially these sources of unsafety are isolated
- **R-S1d** https://raw.githubusercontent.com/kyren/piccolo/ce709eb1dae5c543cbc78e7e12bb80249d88c55f/README.md
  > This method takes a `fuel` parameter which controls how long the VM should run before pausing
- **R-S3** crates:rhai-1.27.0/src/api/limits.rs
  > pub const MAX_CALL_STACK_DEPTH: usize = 64;
- **R-S4** crates:rune-0.14.2/src/runtime/function.rs
  > let mut vm = Vm::new(self.context.clone(), self.unit.clone());
- **R-S4b** crates:rune-0.14.2/src/runtime/budget.rs
  > without explicit co-operation from native functions the budget cannot be enforced
- **R-S5** https://raw.githubusercontent.com/google/starlark-go/4f4267fb140d1e8b91c9bce55cee2f809e8840dd/starlark/interp.go
  > called recursively
- **R-S6** crates:starlark-0.14.2/src/eval/runtime/evaluator.rs
  > DEFAULT_STACK_SIZE: usize = 50;
- **R-S7** https://raw.githubusercontent.com/erlang/otp/OTP-28.1/erts/doc/references/erl_nif.md
  > return to its caller within 1 millisecond
- **R-S7b** https://raw.githubusercontent.com/erlang/otp/OTP-28.1/erts/doc/references/erl_nif.md
  > This approach is always preferred over the other alternatives
- **R-S8** https://raw.githubusercontent.com/wren-lang/wren/0.4.0/doc/site/embedding/calling-c-from-wren.markdown
  > The VM is not re-entrant
- **R-S8b** https://raw.githubusercontent.com/wren-lang/wren/0.4.0/src/vm/wren_vm.c
  > Cannot already be in foreign call.
- **S-S1** repo:src/native/ty.rs
  > a `Val` is valid only for the duration of the synchronous native call
- **S-S2** repo:docs/SPEC.md
  > the engine re-runs a `vm_function` from the start
- **S-S3** repo:src/stdlib/prelude.sand
  > self.sort_by_keys(self.map(key))
- **S-S4** repo:src/lib.rs
  > #![forbid(unsafe_code)]
- **S-S5** repo:src/vm/sched.rs
  > const YIELD_EVERY: Duration = Duration::from_millis(2);
- **W-S10** https://raw.githubusercontent.com/WebAssembly/js-promise-integration/5b247b1026d2f438eefe51914a79f4218cbda9f9/proposals/js-promise-integration/Overview.md
  > Not all applications can equally tolerate being reentrant in this way
- **W-S2** crates:wasmtime-50.0.1/src/runtime/store.rs
  > cannot do blocking `call` from an async context
- **W-S4** crates:wasmtime-50.0.1/src/lib.rs
  > separately allocated native stack
- **W-S5** crates:wasmtime-internal-fiber-50.0.1/src/stackswitch.rs
  > defer the error to happen at runtime when a fiber is created
- **W-S5b** crates:wasmtime-internal-fiber-50.0.1/src/nostd.rs
  > fibers unsupported on this host architecture
- **W-S6** crates:wasmtime-50.0.1/src/config.rs
  > space consumed by a host function is counted towards this limit
- **W-S6b** crates:wasmtime-50.0.1/src/config.rs
  > Exhausting the thread stack typically leads to an abort of the process
- **W-S7** https://raw.githubusercontent.com/WebAssembly/component-model/ef29236d08/design/mvp/CanonicalABI.md
  > ensures that components cannot be externally reentered
- **W-S8** https://raw.githubusercontent.com/WebAssembly/component-model/d230073ce81c6634f175b4d0c5ad900b5253f1f4/design/mvp/Concurrency.md
  > may reenter the caller's component instance
- **W-S8b** https://raw.githubusercontent.com/WebAssembly/component-model/d230073ce81c6634f175b4d0c5ad900b5253f1f4/design/mvp/Concurrency.md
  > must be careful to only perform recursive reentrance on components when explicitly supported by the component's documented API
- **W-S8c** crates:wasmtime-50.0.1/src/runtime/component/store.rs
  > We return `false` here only `self` has been poisoned due to a trap.
