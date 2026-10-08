# Sand implementation internals

This document describes how the implementation is organised and, in detail,
how to write standard-library modules and natives.

## Pipeline

```
source ──lexer──▶ tokens ──parser──▶ AST ──checker──▶ typed HIR ──compiler──▶ bytecode ──VM──▶ effects
 src/syntax/lexer.rs   src/syntax/parser.rs   src/check/   src/hir.rs   src/compile/   src/vm/
```

* `src/engine.rs` — `Engine`, `Config`, module discovery (imports are found
  with the lexer, checked against the policy, read from disk) and compilation
  on a big-stack thread.
* `src/check/` — name resolution, type inference (`unify.rs`), patterns and
  exhaustiveness (`pat.rs`), expressions (`expr.rs`). Produces `CheckedProgram`.
* `src/compile/` — HIR to register bytecode (`src/vm/bytecode.rs`).
* `src/vm/` — values (`value.rs`), GC heap (`heap.rs`), interpreter
  (`interp.rs`), scheduler and async I/O (`sched.rs`), display/format
  (`display.rs`), equality/ordering/hashing (`compare.rs`).
* `src/policy.rs` — capabilities and policies.
* `src/native.rs` — the public `NativeModule` API for hosts.
* `src/stdlib/` — the standard library.

## Standard library modules

A module `foo` consists of:

* `src/stdlib/foo.sand` — the public interface written in Sand: types,
  doc comments (`///`, shown by `help`), pure-Sand helper functions and
  `native fn` declarations (no body).
* `src/stdlib/foo.rs` — `pub fn register(r: &mut Registry)` which registers the
  Sand source and every native:

```rust
use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::value::{Obj, Value};
use crate::vm::{panic, Ctl, NResult, Vm};

pub fn register(r: &mut Registry) {
    r.source("foo", ModKind::Std, include_str!("foo.sand"));
    r.add_builtin("foo.double", |vm: &mut Vm, args: &[Value]| -> NResult {
        Ok(Value::Int(args[0].as_int() * 2))
    });
}
```

### Native names

The registry key of a `native fn` is:

| Declaration | Key |
|-------------|-----|
| top-level `native fn bar` in module `foo` | `foo.bar` |
| `native fn bar` inside `struct Baz`/`enum Baz`/`extend Baz` in module `foo` | `foo.Baz.bar` |
| prelude top-level | `bar` |
| prelude `extend str` / `[T]` / `[K: V]` / `Set<T>` / `int` / ... | `str.bar`, `List.bar`, `Map.bar`, `Set.bar`, `int.bar`, ... |

A missing registration is a compile error: `no native implementation registered for ...`.

### Calling convention

* Natives receive `args: &[Value]` in declaration order; methods receive
  `self` as `args[0]`. Default arguments are always filled in by the compiler,
  so `args.len()` equals the number of declared parameters.
* For `reflect` type parameters (e.g. `native fn decode<reflect T>(text: str)`),
  a `TypeDesc` object (`Obj::TypeDesc(Arc<TypeDesc>)`) is passed **before**
  the declared parameters, one per reflect parameter.
* Return `Ok(value)` or an `Err(Ctl)`:
  * `Ctl::Panic(msg)` — runtime panic (use `crate::vm::panic(msg)`).
  * `Ctl::Limit(LimitKind::Memory)` — memory limit; check big allocations
    first with `vm.reserve(bytes)?`.
  * `Ctl::Async(future)` — suspend the calling task until the future
    completes. The future must be `Send + 'static` and must not hold `Value`s;
    it resolves to a `Finisher` (`Box<dyn FnOnce(&mut Vm) -> NResult + Send>`)
    that builds the result value on the VM thread:

    ```rust
    Err(Ctl::Async(Box::pin(async move {
        let text = tokio::fs::read_to_string(path).await;
        Box::new(move |vm: &mut Vm| -> NResult {
            Ok(match text {
                Ok(t) => { let s = vm.new_str(t); vm.ok(s) }
                Err(e) => vm.err_value(io_kind(&e), &e.to_string()),
            })
        }) as Finisher
    })))
    ```
  * `Ctl::NeedPermission(cap)` — returned by `vm.permit(&cap)?`; the engine
    prompts the user and re-executes the call. Always check permissions
    *before* performing an effect, and propagate with `?`.
  * `Ctl::Block(BlockOn::...)` — scheduler waits (tasks/channels; core only).
  * `Ctl::Yield`, `Ctl::Exit(code)`.

### Values

`Value` is `Copy`: `Unit`, `Bool`, `Int`, `Float`, `Dur` (nanoseconds),
`Tag(variant_id)` (payload-less enum variant) and `Obj(handle)`. Useful
helpers on `Vm`:

* strings: `vm.str(v) -> &str`, `vm.new_str(s)`; bytes: `vm.heap.bytes_of(v)`,
  `vm.heap.new_bytes(vec)`; lists: `vm.heap.list(v)`, `vm.heap.new_list(vec)`,
  `vm.heap.list_push`; tuples: `vm.heap.new_tuple(vec)`, fields of tuples,
  structs and variants: `vm.heap.fields(v)`.
* maps: `vm.heap.alloc(Obj::Map(Box::default()))`, `vm.map_insert(m, k, v)`,
  `vm.map_get(m, k)`; iterate `Obj::Map(m)` → `m.entries.iter().flatten()`.
  Sets: `Obj::Set`, `vm.set_add`.
* `Option`/`Result`: `vm.some(v)`, `vm.none()`, `vm.option(Option<Value>)`,
  `vm.ok(v)`, `vm.err(v)`, `vm.as_option(v)`.
* errors: `vm.error_value(kind, message)` builds an `Error` struct;
  `vm.err_value(kind, message)` builds `Err(Error {..})`. Standard kinds:
  `error`, `not_found`, `permission_denied`, `already_exists`, `io`,
  `timeout`, `parse`, `invalid`, `http`, `closed`, `limit`.
* structs declared in Sand: `vm.heap.alloc(Obj::Struct(def_id, fields))`.
  Find a struct's def id by name with `vm.prog.types.iter().position(|t| t.name == "Metadata" && t.module == "fs")`.
  Enum variants: `vm.prog.variants` (runtime ids), `Value::Tag(id)` for
  payload-less variants, `Obj::Variant(id, fields)` otherwise.
* display: `vm.display(v)`, `vm.debug(v)`.
* Borrowing: `vm.str(a)` borrows the heap; copy out (`.to_string()`) before
  allocating.

### Opaque native types

Declare `pub native type Conn` in Sand, and implement `NativeObject` for a
Rust struct; allocate with `vm.heap.alloc(Obj::Native(Box::new(x)))` and read
with `vm.heap.native::<T>(v)` / `native_mut`. Return `true` from
`is_resource()` for OS resources (sockets, listeners); check
`vm.heap.open_resources < vm.host.limits.max_open_resources` before opening one
(fail with kind `limit`) and call `vm.heap.resource_closed()` when closed
explicitly. Unreachable objects are dropped by the GC, which closes them.

### Engine services (`vm.host`)

`policy`, `cwd` (resolve relative paths against it), `args`, `env`
(`Some(vec)` = virtual environment, `None` = process environment), `rng`
(xoshiro256**), `limits`, `log_level`, `stdin`, and
`vm.write_stream(1 | 2, bytes)` for stdout/stderr (policy and output limit are
enforced there).

## Tests

* `cargo test --test spec_runner <file>` runs `tests/spec/<file>.test`;
  `SPEC_FILTER=substring` limits it to matching cases.
* API, sandbox, network and example tests live in `tests/*.rs`.
