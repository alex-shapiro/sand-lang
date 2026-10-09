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
  on a big-stack thread. Only imported modules are compiled; `help.module`
  summaries of other modules come from `ModuleHelp`, which type checks all
  standard and host modules (without compiling them) on the first request and
  caches the result for the engine's lifetime.
* `src/check/` — name resolution, type inference (`unify.rs`), patterns and
  exhaustiveness (`pat.rs`), expressions (`expr.rs`), the compile budget
  (`budget.rs`). Produces `CheckedProgram`.
* `src/compile/` — HIR to register bytecode (`src/vm/bytecode.rs`).
* `src/vm/` — values (`value.rs`), GC heap (`heap.rs`), interpreter
  (`interp.rs`), scheduler and async I/O (`sched.rs`), fuel metering
  (`meter.rs`), display/format (`display.rs`), equality/ordering/hashing and
  sorting (`compare.rs`).
* `src/session.rs` — REPL sessions: incremental checking and compilation and
  a persistent VM (see "REPL sessions" below).
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
  * `Ctl::Interrupted` / `Ctl::Limit(kind)` from `vm.charge(...)?` — see
    below; always propagate them.
* Never panic on script input. As defence in depth, the scheduler runs each
  interpreter slice (with the natives it calls) and each finisher inside
  `std::panic::catch_unwind` (`src/unwind.rs`; once per slice, not per
  native call), so a Rust panic becomes a panic of the run (`internal
  error: ...`); the public `run_*`/`Session` futures are wrapped in
  `unwind::CatchUnwind`, which turns any other panic into
  `Error::Runtime { message: "internal error: ..." }`.
  The VM may be left inconsistent after such a panic (it is memory safe), so
  the scheduler sets `Vm::caught_panic` and a session resets itself (see
  "REPL sessions").

### Metering: charging fuel and checking limits

The interpreter spends one unit of fuel per call, loop iteration and backward
jump, and the scheduler checks the limits between slices of `SLICE_FUEL`
units. A native runs inside a single instruction, so **any native whose work
grows with its inputs must charge for that work**, or a script could make one
call do unbounded work invisible to `max_instructions`, `max_cpu_time`,
`max_wall_time` and the interrupt handle (SPEC §15.2 gives the cost model):

```rust
vm.charge(items.len() as u64)?;   // 1 unit per element visited/compared/produced
vm.charge_bytes(text.len())?;     // 1 unit per 64 bytes copied/compared/scanned
vm.charge_slow_bytes(data.len())?; // 1 unit per 8 bytes of byte-at-a-time work
```

* Charging is cheap (a subtraction and a comparison on a `Cell` in
  `vm.meter`); it takes `&self`, so it works while the heap is borrowed. No
  syscalls happen until the slice's native fuel (`SLICE_FUEL` units, kept
  apart from the interpreter's own counter) is spent; after that, every
  `CHECK_INTERVAL` units the meter checks the interrupt flag, the instruction
  budget and the CPU/wall deadlines the scheduler computed for the slice, and
  `charge` returns `Err(Ctl::Interrupted)` or `Err(Ctl::Limit(kind))`.
  Propagate with `?`; `call_native` turns them into the run's error.
* The instruction limit is exact for natives. The meter's next checkpoint
  (`check_at`) is never later than the point where the fuel used exceeds
  `max_instructions`: `start_slice` knows the fuel used before the slice and
  the interpreter's fuel for it, and the interpreter reports the fuel it has
  left before each native call (`Meter::sync_bytecode`, one store and a
  compare in the `native_call!` macro), so the comparison in `charge` stays
  the only cost on the hot path. Near the limit the scheduler also shortens
  the interpreter's slice to the remaining budget plus one unit. The
  interpreter's own heavy operations (deep comparison, concatenation, ...)
  see the bytecode fuel as of the last native call, so they may overrun by
  the bytecode spent since (less than a slice); `tests/metering.rs`
  (`natives_stop_exactly_at_the_instruction_limit`) checks the bound.
* **Charge as you go** in loops that can be long (per element, per chunk of
  4 KiB, per match), so the call can be stopped midway. Work that cannot be
  split (one regex search, one `std` call) is charged **before** it starts,
  so it fails fast when the budget cannot cover it.
* Work whose cost cannot be predicted from its size is also charged by time:
  regex searches may scan far beyond the text they report or fall back to a
  slower engine, so `stdlib/regex.rs` (`Scan`) raises each call's charge to
  one unit per microsecond it has taken (far above the cost of ordinary
  searches, whose fuel therefore stays deterministic).
* Code that only has `&Heap` takes a budget: `meter::Budget` is implemented by
  `Meter` and by `Unmetered` (for callers outside an engine run). The
  `compare` and `display` modules provide `*_metered` variants
  (`values_eq_metered`, `values_cmp_metered`, `hkey_metered`,
  `with_keyref_metered`, `to_display_metered`, ...); `vm.display`/`vm.debug`
  and the `vm.map_*`/`vm.set_*` helpers already charge.
* APIs that report errors as `String` (map/set helpers, `vm.arith`) encode
  limit errors with `meter::ctl_message` (a NUL-prefixed code); convert them
  with `.map_err(meter::ctl_from_message)`, which turns them back into
  `Ctl::Limit`/`Ctl::Interrupted`. Never use `.map_err(Ctl::Panic)`: a
  `Ctl::Panic` is always reported as a panic, whatever its text, so that a
  script's `panic("\u{0}memory")` cannot pass for a limit error.
* Sort with `compare::try_sort_by`, a stable merge sort whose comparator may
  fail (charge per comparison). The standard library's sorts cannot be stopped
  midway, and may panic if a comparator becomes inconsistent.
* A native that spends more than the slice's native fuel preempts its task
  when it returns (other tasks run, the scheduler checks the limits).
* Host modules: typed `NativeModule::function`s are charged for converting
  their arguments; `raw_function`s receive `&mut Vm` and should call
  `vm.charge` like built-in natives.

### Scratch memory

`max_memory` counts heap objects. Rust-side buffers a native builds before
turning them into values (output strings, piece lists, encodings, sort
buffers, hash keys) must be checked too, so a script cannot make the host
allocate far beyond the limit:

* Reserve **before** allocating when the size is known: `vm.reserve(bytes)?`
  (e.g. `repeat`, `join` and `replace` compute their exact output size;
  `split` counts the pieces first).
* Reserve **before mutating** anything visible to the script. `vm.reserve`
  compares against the heap's byte count, which includes garbage a native
  cannot collect (its Rust locals are not GC roots), so when a native fails
  with the memory limit the interpreter collects garbage and calls it once
  more; the same happens for the memory checks of heavy instructions (the
  scheduler re-runs the instruction after a collection).
* When a buffer grows incrementally, check as it grows: `meter::Scratch`
  checks `heap.would_exceed(len)` every `SCRATCH_STEP` bytes of growth
  (`render_metered`, JSON encoding and regex iteration do this).
* Beware of shared structure: a value whose nodes reuse the same child has
  a display form, JSON text or hash key exponentially larger than its heap
  size.

### Values

`Value` is a 16-byte `Copy` enum: `Unit`, `Bool`, `Int`, `Float`, `Dur`
(nanoseconds), `Tag(variant_id)` (payload-less enum variant), `Obj(handle)`,
and the *inline variants* `VUnit`, `VBool`, `VInt`, `VFloat`, `VDur`, `VTag`
and `VObj`, which hold a runtime variant id plus a single payload. Every enum
variant with exactly one payload is stored inline — without a heap
allocation — unless the payload is itself an inline variant: `Some(1)` is
`VInt(some, 1)`, `Ok("x")` is `VObj(ok, handle)`, `Some(None)` is
`VTag(some, none)`, but `Some(Some(1))` and variants with two or more fields
are heap `Obj::Variant`s. The representation is canonical, so equality,
ordering and hashing may compare representations directly, and so natives must
never build `Obj::Variant` by hand: use `vm.some`/`ok`/`err`/`option` or
`vm.heap.new_variant(id, &fields)`. To inspect variants independently of their
representation use `vm.heap.variant_id(v)`, `vm.heap.variant(v)` (a
`VariantRef` with `id()` and `fields()`), `vm.heap.field(v, i)`,
`vm.variant_payload(v, i)`, `vm.as_option(v)` and `vm.as_result(v)`; note
that `vm.heap.fields(v)` and `vm.heap.get(v)` only see heap objects. A value
refers to a heap object through `Obj(h)` or `VObj(_, h)` (`Value::handle()`).
Useful helpers on `Vm`:

* strings: `vm.str(v) -> &str`, `vm.new_str(s)`; bytes: `vm.heap.bytes_of(v)`,
  `vm.heap.new_bytes(vec)`; lists: `vm.heap.list(v)`, `vm.heap.new_list(vec)`,
  `vm.heap.list_push`; tuples: `vm.heap.new_tuple(vec)`, fields of tuples and
  structs: `vm.heap.fields(v)` (for variants see above).
* maps: `vm.heap.alloc(Obj::Map(Box::default()))`, `vm.map_insert(m, k, v)`,
  `vm.map_get(m, k)`; iterate `Obj::Map(m)` → `m.entries.iter().flatten()`.
  Sets: `Obj::Set`, `vm.set_add`.
* `Option`/`Result`: `vm.some(v)`, `vm.none()`, `vm.option(Option<Value>)`,
  `vm.ok(v)`, `vm.err(v)`, `vm.as_option(v)`, `vm.as_result(v)`; any
  variant's payload: `vm.variant_payload(v, i)`.
* errors: `vm.error_value(kind, message)` builds an `Error` struct;
  `vm.err_value(kind, message)` builds `Err(Error {..})`. Standard kinds:
  `error`, `not_found`, `permission_denied`, `already_exists`, `io`,
  `timeout`, `parse`, `invalid`, `http`, `closed`, `limit`.
* structs declared in Sand: `vm.heap.alloc(Obj::Struct(def_id, fields))`.
  Find a struct's def id by name with `vm.prog.types.iter().position(|t| t.name == "Metadata" && t.module == "fs")`.
  Enum variants: `vm.prog.variants` (runtime ids); build values with
  `vm.heap.new_variant(id, &fields)` (a `Value::Tag` when there are no
  fields, inline for a single payload where possible, `Obj::Variant`
  otherwise).
* display: `vm.display(v)`, `vm.debug(v)`.
* Borrowing: `vm.str(a)` borrows the heap; copy out (`.to_string()`) before
  allocating.

### Opaque native types

Declare `pub native type Conn` in Sand, and implement `NativeObject` for a
Rust struct; allocate with `vm.heap.alloc(Obj::Native(Box::new(x)))` and read
with `vm.heap.native::<T>(v)` / `native_mut`. Return `true` from
`is_resource()` for OS resources (sockets, listeners); check
`vm.heap.open_resources + vm.heap.io_memory.pending_resources() <
vm.host.limits.max_open_resources` before opening one (fail with kind
`limit`) and call `vm.heap.resource_closed()` when closed explicitly. A
resource that a pending operation opens off the VM thread (a connection
being established, an accepted connection, an HTTP request's socket) holds
an `iomem::ResourceSlot` (`net::resource_slot(vm)` checks the limit and takes
one) from when the operation starts until it fails, finishes or is
cancelled, or until the finisher turns it into a heap object
(`net::alloc_resource` drops the slot right before allocating). The fixed
buffers of a connection are reserved with its slot: `net::conn_slot(vm,
bytes)` takes a slot and reserves `bytes` (`net::CONN_BUFFER`, 8 KiB, the
read buffer; plus `net::TLS_BUFFERS`, an estimate of a TLS session's
buffers; HTTP server connections have two read buffers), failing with kind
`limit` if they do not fit; `Conn::size()` then counts them while the
connection is open, and `Conn.close` accounts for their release
(`vm.heap.account`). Unreachable objects are dropped by the GC, which closes
them.
`size()` should report the memory the object owns (`max_memory` counts it);
an object whose memory grows after allocation (a compiled regex's search
cache) reports its new size and the native accounts the change with
`vm.heap.account(before, after)`, so that `size()` always equals what was
counted.
Native objects must not hold `Value`s: the collector does not trace them, and
`native_mut` bypasses the write barrier.

### Engine services (`vm.host`)

`policy`, `cwd` (resolve relative paths against it), `args`, `env`
(`Some(vec)` = virtual environment, `None` = process environment), `rng`
(xoshiro256**), `limits`, `log_level`, `stdin`, and
`vm.write_stream(1 | 2, bytes)` for stdout/stderr (policy and output limit are
enforced there).

### I/O natives and the sandbox

* **Files**: resolve the script's path with `resolve_path` (symlinks
  followed; use `policy::normalize_entry_path` for operations on a directory
  entry itself), check `Capability::FsRead`/`FsWrite` with `vm.permit` (or
  `vm.permit_entry` for entry operations), then perform the I/O only through
  `stdlib::fs_safe` on the blocking pool. Never use path-based `std::fs` /
  `tokio::fs` calls on script paths: `fs_safe` opens every directory without
  following symlinks (`openat` + `O_NOFOLLOW` per component, `O_PATH` on
  Linux, one `O_NOFOLLOW_ANY` open on macOS; on Windows `NtCreateFile`
  relative to the parent's handle with `FILE_OPEN_REPARSE_POINT`, through
  the `fs_at` crate, refusing symlinks and junctions), so the effect lands
  exactly on the checked location. On Windows, creating, removing and
  listing go through handles too; only `rename` is path-based, holding every
  directory on both paths open without `FILE_SHARE_DELETE` (each checked
  against the handle walk by file ID, `same-file`) so that none can be
  renamed or replaced, and checking that neither parent became a junction
  just before and after. Windows names that Win32 would alter (trailing `.`
  or space, `:` streams, `CON`, ...) are refused, since handle-relative opens
  take them literally. Operations that move or remove whole trees must also
  refuse when `vm.host.policy.denies_under(path)`. In `fs.rs`, the
  `target!`/`entry_target!` macros do this resolution: they refuse paths
  over `policy::MAX_PATH_BYTES` (kind `invalid`) and charge for the
  normalisation, which `normalize_path` keeps to O(log n) `canonicalize`
  calls by binary search, and for the directory entries read to find a
  symlink's stored name (`policy::entry_path`, at most
  `policy::MAX_LINK_SCAN`). Denial errors name the path as the script spelled
  it (`denied`), never the resolved path.
* **Path rules**: allow rules compare normalised paths exactly; deny rules
  compare them through `policy::fold_name` (NFD, case folding, NFD,
  without default-ignorables), which equates every spelling a case- or
  normalisation-insensitive file system may treat as the same name, so deny
  rules also hold for paths that do not exist yet. `normalize_entry_path`
  names an existing non-symlink entry as stored (`canonicalize`), and a
  symlink by reading its (canonical) directory for the entry whose name
  folds the same and that is the same entry (device and inode, or Windows
  file ID), preferring an exact match; past `MAX_LINK_SCAN` entries the
  given spelling is kept (deny rules still match it).
* **Prompts**: a native returning `Ctl::NeedPermission` is retried after the
  prompt; "once" answers (`host.grants_once`/`denies_once`) stay valid while
  the same call is retried (natives checking several capabilities ask
  several times) and `call_native` forgets them as soon as the call returns
  anything else (`Vm::forget_once_answers`).
* **Memory**: buffers that pending I/O fills off the heap must be reserved
  (`vm/iomem.rs`): start with `vm.io_reservation()` (or `vm.reserve_io(n)`,
  whose `Err` is the `Err(Error { kind: "limit" })` value to return) on the
  VM thread after the permission check, grow it in the future or blocking
  task as data arrives with `res.reserve_buf(&mut buf, extra)` /
  `res.ensure(total)` (`MemoryLimit` converts into `NetError` and
  `io::Error`, which become errors of kind `"limit"`), move it into the
  finisher and drop it right before `vm.reserve(len)?` and allocating the
  value. Never reserve a size a peer merely announces (e.g.
  `Content-Length`) before the data arrives. Reserved bytes count in
  `Heap::would_exceed` / `over_limit`; the scheduler publishes the heap size
  for checks made off the VM thread.
* **Network**: check `Capability::Net { host, port }` with `vm.permit`, then
  connect with `net::connect_checked` / `net::tcp_connect` (passing a clone
  of `vm.host.policy`): they resolve the name once, check each address as
  `Capability::NetAddr` and connect only to allowed addresses. TLS goes
  through `net::tls::connect(&vm.host.tls, host, tcp)`, which honours
  `Config::tls_root_certificates`; bound it (and every other wait on a
  peer) with a timeout. Take a `net::resource_slot` before connecting (see
  "Opaque native types").

## Traits

Runtime values are erased, so generic code with trait bounds (spec §4.9)
receives **dictionaries**: evidence that a type implements a trait, passed as
hidden arguments. `src/check/traits.rs` holds the checker side.

### Declarations

* A trait (`TraitDef`) has its own type parameters, a type parameter for
  `Self` (bounded by the trait itself, so default methods can call trait
  methods on `self`), supertraits and methods. Each method has a signature
  function (`FnKind::TraitMethod(trait, slot)`, no body) and optionally a
  default function, a normal script function with the same signature.
* An impl (`ImplDef`) records its type parameters, the trait arguments, the
  target type, its *evidence* (the trait bounds of its type parameters) and
  `slots`: for each trait method, the impl's function or the trait's default.
  Impls are registered in `Checker::impls_by[(trait, type constructor, tuple
  arity)]`; coherence allows at most one impl per entry with unifiable trait
  arguments.

### Calling convention

* `FnInfo::dicts` lists the hidden dictionary parameters, which precede the
  declared ones (`HFn::params` includes them). For an ordinary generic
  function they are one per (type parameter, trait bound), in declaration
  order. For trait methods (signatures, defaults and impl methods) the first
  is the dictionary of `Self`, followed by the bounds of the method's own type
  parameters.
* A dictionary is a tuple: the trait's method functions (in declaration
  order), then the dictionaries of the trait's supertraits for the same type,
  then (for impls) the dictionaries of the impl's evidence. A supertrait's
  dictionary is reached by projecting a field; impl methods read the evidence
  of their impl's bounds from fields of their first parameter.
* Default argument helpers of a function with dictionaries take the same
  dictionaries (`HArg::Default::inputs` is shifted accordingly).

### Evidence resolution

While checking a body, every call that needs evidence gets an
`HKind::Evidence { ty, trait }` placeholder argument, since `ty` may still be
an inference variable. `finish_fn` resolves the placeholders first (before
built-in bound obligations, as resolution can determine types), replacing
each with:

1. for a type without type parameters, the impl for its constructor: an
   `HKind::Dict { impl, args }` whose arguments are the recursively resolved
   supertrait and impl evidence. Candidate impls are tested by tentative
   unification (`unify_probe`, which undoes bindings through
   `Checker::unify_log`);
2. otherwise a *fact* of the function: a hidden parameter (`HKind::Local`),
   or a field projection of one for supertraits and impl evidence
   (`FnState::evidence`), falling back to impls.

Resolving one placeholder can determine the type of another, so resolution
repeats while it makes progress; only then are failures reported.

When a call's evidence is created and a single impl or bound applies, the
impl's or bound's trait arguments are unified immediately ("improvement"), so
`let f = celsius.into()` infers the target type from the only impl. Function
values of generic functions with bounds and trait methods (`xs.map(show)`,
`Point.describe`) become closures (`HLambda`) that capture the resolved
dictionaries and call the function; lambdas, local functions and `spawn`
capture dictionaries like any other local. If such a closure's evidence is
for a type that is never inferred but occurs in the closure's parameter types
(as in `help.doc(Describe.describe)`), no argument of that type can exist, so
the closure can never run and a unit placeholder is passed instead
(`EvidenceReq::lenient`).

### Code generation

* `HKind::CallTrait { slot, args, impl_id }`: when the dictionary comes from
  a known impl (`impl_id`, noted when the placeholders are resolved), the
  compiler emits a direct `CallDirect` to that impl's method; otherwise it
  loads the method with `GetField` from the dictionary and emits `Call` (no
  hashing or lookup).
* A `Dict` whose arguments are all `Dict`s is a compile-time constant
  (`Const::Tuple`, built when the VM starts and deduplicated). A `Dict` that
  depends on the function's own dictionaries (a generic impl used from
  generic code, like `[T]: Describe`) is built with `NewTuple` where it is
  used, except that those used in loops or closures are built once on entry
  to the function (shared, in hidden locals).

So a trait method call on a concrete type costs the same as an inherent
method call plus one constant load; a call through a bound costs a field load
and an indirect call; a generic impl used from generic code allocates its
dictionary (a small tuple) once per use, or once per call of the function
when used in a loop or closure.

### `Num` evidence

The built-in bound `Num` carries evidence too, because erased values cannot
tell a native which zero to return for an empty `[float]`. The prelude
declares a private trait `Num` (static `zero` and `one`, implemented for
`int` and `float`; only the prelude may declare or implement it).
`Checker::add_bounds` gives every type parameter with an explicit `Num` bound
that trait as well, so such functions take a hidden `Num` dictionary and
`T.zero()` resolves through it; a trait with the built-in supertrait `Num`
gets the trait as a real supertrait. Struct type parameters do not keep the
trait (their methods' copies of the parameters do). Failing `Num` evidence is
not reported separately: the built-in bound check already reports it (except
for types that stay unknown). `[T].sum`/`product` are Sand functions passing
`T.zero()`/`T.one()` to the natives `sum_from`/`product_from`.

Trait objects (`dyn Trait`) could later be represented as a pair of a value
and its dictionary built at the coercion site, reusing the same layout.
## Compiler

### Loop-invariant hoisting

Before each outermost loop (`loop`, `while`, `while let`, `for`), the
compiler (`hoisted_loop` in `src/compile/mod.rs`) loads loop-invariant values
into registers reserved for the whole loop nest, and `expr_reg` returns those
registers instead of re-materialising the values on every iteration:

* constants (`int`, `float`, `str`, `bytes`, `duration`, payload-less
  variants) used as instruction operands; ones compiled as immediates
  (`x + 1` → `AddIK`) or materialised into a fixed register anyway (call
  arguments, constructor fields) are left alone;
* immutable (`let`) globals, in any position. Reading a global before its
  initialiser has run must panic at the point of use, so these are loaded
  with `HoistGlobal { dst, g, off }`, which jumps to a second, unhoisted copy
  of the loop if the global is not initialised yet. Immutable globals never
  change once initialised, so the hoisted copy is exact. This loop
  versioning is skipped for loops containing lambdas (their prototypes would
  be duplicated), loops larger than `MAX_VERSIONED_NODES` HIR nodes, and in
  the unfused fallback for very large functions.

`var` globals and captured mutable locals (cells) are never hoisted. At most
`MAX_HOISTED` values are hoisted per loop nest (most frequently used first),
and none when the function already uses `HOIST_REG_BUDGET` registers. Inspect
the result with `SAND_DUMP_BYTECODE=1 sand run file.sand`.

## Virtual machine

### Dispatch loop

`Vm::interp` is one `match` on the instruction, and its speed depends on how
LLVM allocates registers across that whole function: when too many values
stay live across the loop (it contains calls, so they need callee-saved
registers), the allocator spills the hottest ones, `code` and `ip`, and
every instruction reloads them from the stack (±10-40% on call-heavy code
from unrelated edits). The rules that keep it fast:

* Keep the loop-carried state minimal: `code`, `ip`, `base`, `regs` and the
  fuel; read anything else from memory where it is needed (the current
  frame's `ret`/`closure` from `fiber.frames`, the depth limit from
  `self.host.limits`, the `Some` variant id from `prog.ids`). Adding one
  more long-lived local can push `code` or `ip` out of registers.
* Keep work out of the arms: anything with a loop, an allocation of many
  values, formatting or a second call goes into an `#[inline(never)]`
  method (`make_closure`, `op_compare`, `op_display`, `op_concat`,
  `str_class`, `iter_step`, `call_native`, `try_top`, ...), and error paths
  into `#[cold]` functions that build the message (`operand_trap` for the
  operand checks of every typed arithmetic instruction, `index_error`, ...).
* Measure: `cargo bench` prints the geometric mean of the benchmarks' best
  times; compare it before and after any change to the loop, and inspect the
  dispatch head with `objdump -d --disassemble-symbols=<Vm::interp>` (look
  for `ldr`/`str` of `sp`-relative slots around the `br` of the jump table).

Moving all rare instructions into one out-of-line `exec_cold` handler (a
smaller loop) was measured and is slower (geometric mean 78 ms against 68
ms): the fallback arm's call and result handling cost more registers than
the arms it removed. A threaded design (one function per instruction,
tail-calling the next) needs guaranteed tail calls, which stable Rust does
not have.

The compiler reduces the number of instructions dispatched: fused
compare-and-branch instructions (also for `or` and `not` of integer
comparisons, `cond_jump_true`, and character-class tests, `JStrClass`),
`JNotSome` (test and payload of `Some` in one; after a call of an
`Obj::IterFn`, `CallR` performs a following `JNotSome` itself), calls
writing their result straight to the destination register (`CallDirectTo`,
`CallNativeTo`, `Call`'s `dst`), calls of a function already in a register
without moving it into the call's base (`CallR`), one-argument natives and
single-payload variants reading their argument in place, statements not
materialising unit, captures loaded once per closure call, inline
instructions for small natives (`Len`, `ListPush`, `IntToFloat`,
`StrClass`), and jump threading (`thread_jumps`: a jump to a jump, a `Loop`
or a return is replaced by its target).

### Strings built by appending

`s += x` on a string local copies `s` each time, which makes building a
string piece by piece quadratic (CPython avoids this by appending in place
when the string has no other reference). So the compiler appends in place
when it can prove no one else sees the string: for `var` locals of type
`str` that are appended to and not captured by a closure (not cells),
`builder_locals` makes `s += x` an `AppendStr`, which keeps the value in a
private, growable `Obj::StrBuf` (amortised doubling) held only by the
local's register; every other read of the local first emits `FreezeStr`,
which turns the buffer into an ordinary `Obj::Str` in place (same handle).
So a buffer never reaches another register, object, closure or native, and
appending can never change a value seen elsewhere. The buffer's capacity
counts against `max_memory`; freezing gives back the unused part.

### Fibers, frames and stacks

Every task runs on a `Fiber` with its own value stack and frame list. A
`Frame` is 20 bytes: `proto`, `ip`, `base` (first register), `ret` (absolute
slot receiving the return value) and `closure` (a heap handle, or
`NO_CLOSURE` for direct calls). The running fiber is taken out of
`vm.fibers` while it executes, so natives never see it.

* The stack starts at 64 slots and grows geometrically (`Fiber::grow_stack`)
  up to `MAX_STACK_SLOTS`; returning from a call does not shrink it.
* Only the *live window* of each fiber — up to the innermost frame's
  `base + nregs` (`Fiber::live_top`) — is a GC root. Slots above it are dead:
  the collector clears them (so they never hold handles to freed objects) and
  shrinks stacks left large by deep recursion. Consequently the compiler must
  never read a register before writing it in the same frame.
* Finished fibers are kept for reuse (`Vm::recycle_fiber`, up to
  `MAX_SPARE_FIBERS` with small stacks): spawning a task then allocates no
  stack or frame list. The pool counts in `fiber_bytes` and is freed before
  a memory limit error is reported (`free_spare_fibers`). A recycled stack
  is cleared, since the new task's live window is a GC root before the task
  has written all of it.
* The scheduler reads the clock (for CPU time) after every `CLOCK_SLICES`
  slices or `SLICE_FUEL` units of fuel and around every wait, not once per
  slice: tasks that finish or block quickly make slices far shorter than
  a clock reading.
* The interpreter keeps the current register window as a local slice
  (`regs`) and the fuel counter in a local; `reload!()` re-derives the window
  after anything that may move or resize the stack (calls, returns, natives,
  collections). Natives receive their arguments as a slice of the stack, with
  no copy.

### Garbage collector

The heap (`src/vm/heap.rs`) is an arena of `Obj`s addressed by `u32` handles
with a free list; objects never move. Collection is precise and happens only
at safe points in the interpreter (loop back-edges and calls, when
`heap.wants_gc()`), never inside natives, so natives may keep `Value`s in Rust
locals. Roots are globals, constants, `vm.pinned`, and for every fiber its
live stack window, frame closures, task handle and race list.

The collector is generational with *sticky mark bits*: a set mark bit means
the object survived a collection (old); objects allocated since the last
collection are young and listed in `Heap::young`.

* **Minor** collections mark from the roots and the *remembered set* without
  traversing old objects, then sweep only the young list; survivors keep
  their mark (promotion). They run every `NURSERY` (8 MB) of allocation;
  heaps whose live size is below that only see major collections.
* **Major** collections clear every mark, trace the whole heap and sweep the
  whole arena. They run when `bytes` exceeds `major_at` — twice the live size
  after the previous major collection, capped near the memory limit — and
  right after a minor collection that leaves the heap over the limit (so
  `over_limit()` is only reported after a full collection).
* **Write barrier**: `Heap::get_mut` (and everything built on it: `list_mut`,
  `list_push`, `Vm::map_insert`/`map_remove`/`set_add`/`set_remove`, cells,
  struct fields, channels, task results) adds an old object to the
  remembered set on its first mutation after a collection. Any code that
  stores a `Value` inside a heap object must obtain it through these
  accessors; never cache a `&mut Obj` across a safe point. The interpreter's
  hot store paths (list push and index stores, struct fields, cells, map and
  set inserts) use the precise `get_mut_storing(obj, &stored)`, which only
  remembers the object if a stored value refers to a young object, so e.g.
  a large old list that keeps receiving integers is never rescanned.
* Card marking for lists: an old list that only receives young values
  through `Heap::list_mut_storing` (index stores) and `Heap::list_push` is
  *partially* remembered; the minor collection scans only the stored indices
  and the pushed tail (`DirtyList`). Any other mutation, or more than
  max(1024, len / 8) stored indices, turns it into a full rescan.
* Accounting: `bytes` grows on allocation and container growth; minor
  collections subtract the sizes of freed young objects, major collections
  recompute it exactly. Maps and sets count the bytes their key index owns
  (`key_bytes`: strings and `HKey` nodes, copied per entry, so a key tuple
  repeating a shared string counts each copy). `fiber_bytes` counts the
  fibers (stacks, frames, waiter and race lists, `Fiber::footprint`): it
  grows with them (`Fiber::grow_stack`/`grow_frames` check the limit first)
  and every collection recomputes it. `Heap::used()` (both) is what
  `would_exceed`/`over_limit` compare with the limit. Wait lists (task
  waiters, channel receivers) are pruned of stale entries left by cancelled
  tasks when they reach twice their length after the previous pruning. `open_resources` is decremented for every freed
  open native resource and recounted by major collections.
* `GcStats` (collections, majors, total/max pause, max major/minor pause) is
  available from `Engine::gc_stats()` after a run; `cargo bench` prints it.
* `Config::gc_stress(true)` (or the `SAND_GC_STRESS` environment variable,
  also honoured by the CLI) collects at every safe point, alternating seven
  minor collections with one major one.
## Compile budget

Type checking runs before any runtime limit applies, so the checker carries a
`check::budget::CompileBudget` (`checker.budget`, `Cell`-based like the
meter). Code in `src/check/` that can do superlinear work charges it:
`self.budget.charge(units)` returns `false` once compilation must stop
(`max_compile_fuel` exhausted, `max_wall_time` passed, or the engine
interrupted); callers then bail out cheaply (return `Type::Error`, `err(..)`,
assume success) and the engine reports the stop instead of the diagnostics.
Currently charged: every expression (`infer_inner`), unification and occurs
steps, bound checks, type nodes zonked or measured, and exhaustiveness work.
The bytecode compiler gets the same budget (`CompileInput::budget`): it
charges every expression compiled (`expr_to`, which returns at once when the
budget is exhausted, so the rest of the program is skipped; deferred code
copied to every exit and loops compiled twice for hoisting are charged per
copy), every instruction emitted, and the nodes walked by its analyses
(hoisting candidates, lambda captures); callers check `budget.stopped()`
after `compile`/`compile_more` and report the stop instead of the result.

The formatter (`fmt::format_source_with`) and the linter (`lint.rs`) use a
`CompileBudget` too. Standalone (`sand fmt`, `sand lint`, `format_source`,
`lint_file`, `lint_source`) it is `budget::tool_fuel(len)`: 64 units per
byte of source, at least `DEFAULT_COMPILE_FUEL`, so that their time (and the
formatter's output, charged per byte) stays linear in the source size
whatever its shape. The formatter charges syntax tree nodes, layout nodes
examined by `needs_break`, `contains_hard`, `mark_hug`, `fits` and `render`,
text measured and output printed; the linter charges syntax tree nodes and
the re-scans of loop bodies (`breaks_to`, `uses_label`). Once stopped they
skip the rest and return `source too complex to format`/`lint` (or the time
or interrupt error). Per-binding lookups are kept constant time instead
(`SourceFile::line_col` finds columns from per-256-byte character counts;
the linter's shadowing check keeps a map of the bindings in scope), and the
parser finds line numbers by binary search.
Local limits with precise diagnostics: `limit_type_size` (types of
expressions and aliases, `MAX_TYPE_SIZE` components), the per-`match`
pattern work (`start_pattern_check`/`pattern_step`, `MAX_PATTERN_WORK`),
reflection descriptions (`compile::reflect::MAX_DESC_NODES` per description,
`MAX_TOTAL_DESC_NODES` per compilation), and the lexer's interpolation
nesting (`MAX_STRING_NESTING`).

Reflection descriptions (`TypeDesc`, built by the compiler, not the checker)
are DAGs: `compile::reflect::DescCache` (kept in `compile::Incremental`, so
shared by the inputs of a REPL session) describes each struct or enum
instance once, keyed by a structural key of its `Type`, and shares the
`Arc<TypeDesc>`; each reflected type becomes a single constant used by all
its call sites. A recursive reference is `TypeDesc::Recur(id)`, resolved by
`json.decode` against the enclosing `Struct`/`Enum` with that `id` (ids are
per instance, so same-named types of different modules never mix). A
description containing a `Recur` to an enclosing instance outside it is
only reused while its parent instance is being described; closed ones are
cached for the whole program. Never walk a `TypeDesc` as a tree without
bounding the work (its `Debug` form stops at structs and enums).

## REPL sessions

`src/session.rs` implements `Session` (`Engine::session`, used by `sand repl`):
inputs are checked and compiled incrementally and run once each in one
persistent VM. The design rests on one invariant: **nothing that a live value
can refer to is ever renumbered**. Closures hold proto ids, structs and
variants hold type and variant ids, code holds global slots, constant indices
and native ids, so a session only ever *appends* to the checker's tables and
to the `Program`.

### One module, many inputs

A session is a single script module (`repl`, kind `Script { main: true }`)
whose items arrive one input ("cell") at a time. `Checker::check_cell` checks
an input as more items of that module (`check_module` with `cur_mod` = the
session module), after the modules it imports for the first time (appended
as new modules, numbered from `mods.len()`). Sharing one module scope makes
everything that is module-relative just work across inputs: assigning a `var`
of an earlier input, `extend` of an earlier type, private items, the orphan
rule for impls. What differs in a session module (`Checker::in_session`):

* `define`, `register_method` and `declare_impl` let a definition shadow or
  replace one made by an **earlier input** (told apart by its span's file:
  each input is its own source file, `<input N>`), instead of reporting a
  duplicate or conflicting impl. Replacing only edits the lookup tables
  (`scope`, `methods`, `impls_by*`); the old `FnId`s, `TypeDefId`s and
  `ImplId`s stay, so code checked earlier keeps using them (definitions are
  early-bound, as in ML toplevels).
* Every top-level `let`/`var` becomes a global (normally only those that
  functions or other modules can see do), since later inputs may use it.
* With `want_cell_value`, a final expression statement is inferred as a
  value and becomes the tail of the input's initialiser, whose result the
  session prints (`cell_value` holds its type).
* Globals are finalised (zonked, must be fully inferred) per input.

### Keeping the checker

`Checker<'a>` borrows the `SourceMap` and the native lookup. Between inputs
the session stores it as `Checker<'static>` (`detach` swaps the borrows for
static empty ones; `attach` restores real ones, by covariance). Each input is
checked on a **clone** of the stored checker, so a rejected input leaves no
trace; on success the clone replaces it. To keep clones cheap,
`release_bodies` drops the HIR bodies and syntax of functions once they are
compiled (only new functions are compiled, and checking never looks at other
bodies), and lint facts are cleared per input. Cloning costs about a
millisecond for a session with thousands of definitions.

`take_checked`/`restore_checked` move the checked tables into a
`CheckedProgram` for the compiler and back.

### Appending to the program

`compile::compile_more` is the compiler's core (`compile` is a call to it with
an empty `Program`): it appends the types, variants, global names and protos
of everything added to the `CheckedProgram` since the last call, compiles the
new functions, and returns a fresh `<entry>` proto that calls the given
initialisers (the new modules', then the input's) and returns the last
result. `compile::Incremental` keeps the compiler's tables between calls (fn
id → proto, constant deduplication, variant wrappers, constant dictionaries)
and is cloned per input. The session's `Program` lives in the VM's
`Arc<Program>`; it is extended in place with `Arc::make_mut` (no copy while
the VM holds the only reference), and `ProgramMark` truncates it again if
compilation fails. `Vm::load_program` then creates the values of the new
constants and slots for the new globals.

### Modules and imports

`ModuleGraph` (`src/engine.rs`) holds the sources and the loaded modules of a
compilation; `Engine::discover` loads, depth first and with policy checks,
whatever a module imports that the graph does not hold yet. `Engine::compile`
runs it once from the main module; a session keeps its graph and runs it from
each input (a pseudo-module that is not checked as such), so standard, host
and script modules imported mid-session are loaded, checked, compiled and
initialised once, before the input. `GraphMark` undoes the additions of a
rejected input.

### Running inputs

Each input's entry runs as the main task (`run_main`) of the session's VM,
whose heap, globals, fibers and policy decisions persist. After it finishes,
`release_main` frees its fiber; other tasks stay in the scheduler and run
whenever a later input runs, or the front end idles (below). If the input
fails (panic, limit, interruption,
`exit`), `abandon_tasks` drops every fiber and pending I/O operation and marks
their tasks cancelled; heap and globals are kept. `fuel_used` and
`output_bytes` are reset per input, and `run_main` measures CPU and wall time
per call, so those limits are per input. `Vm::session` makes a failed
top-level `?` panic instead of ending the program. "Always" answers are
shared with the engine (`prepare_run`/`finish_run`).

### Tasks between inputs

`Session::run_tasks_until(until, on_error)` drives the VM while a front end
waits for input. The scheduler only runs while `run_main` runs, so an idle
run is a `run_main` of a hidden entry (`State::idle_proto`, appended to the
program when the session starts) that calls a host native (`<session
idle>`, appended to the session VM's copy of the native table; no `native
fn` can name it) which suspends on a `Gate`. The method polls `until` and
the run together; when `until` completes it opens the gate, the idle main
task finishes at the next scheduling point (within a slice and the 2 ms I/O
poll), and `release_main` frees it. The VM and the scheduler needed no
changes. When no task is ready and no I/O is pending, nothing can progress
and `until` is simply awaited.

During an idle run the session swaps in idle settings, restored from the
engine's configuration by the next `run`: no wall-time limit
(`host.limits.max_wall_time`; fuel, CPU time and output count from zero as
for an input), and a `DeferredPrompter` that waits for the gate before asking
the real prompter, so a background task's prompt never competes with the
front end for the terminal (no script code runs while a prompt is pending,
as always). It also holds the stdin mutex (`try_lock_owned`; skipped if a
read is already in progress), so script reads of standard input wait too.
Failures and `exit` are concluded as for inputs (`conclude`). If an idle
future is dropped, `State::recover` opens its gate on the next call: the
idle main task then finishes as an ordinary task (`main_fiber` has moved on),
and the other tasks are kept; a dropped `eval` (`running`) still cancels
all tasks.

### Internal errors

A Rust panic caught by the scheduler (in a slice or a finisher) sets
`Vm::caught_panic`; one caught anywhere else in a session call
(`CatchUnwind` around `eval`, `run_tests` and the idle run, `unwind::catch`
around `type_of`) means the session's own state may be half-updated. Either
way `Session::settle` resets the session (`reset`, which first records the
VM's "always" answers with the engine, as `finish_run` would have) and
appends `RESET_NOTE` to the error; `run_tests` stops at the test that hit
it. Panics in the checker or compiler (on the compile thread) need no reset:
they work on clones that are discarded, and the program is truncated
(`ProgramMark`), so they just fail the input (`internal compiler error`).

### Front end

The CLI (`cmd_repl` in `src/main.rs`) reads lines on a thread, one per
request, so it can wait for a line and for Ctrl-C (`tokio::signal::ctrl_c`)
at once without reading ahead of a prompter that uses standard input;
`Session::is_complete` (lexer based) decides when an input is complete. It
waits for each line inside `run_tasks_until`. The scripts' standard output
and error go through `ReplSink`s that share a `PromptLine`: while a prompt is
shown, output first moves to a new line and is followed by the prompt again
(text the user had typed stays in the terminal's line buffer but is not
redrawn). Ctrl-C at an empty prompt calls `cancel_tasks`.

### Types in messages

`Session::type_of` checks with `Checker::type_query` set: `instantiate_fn_with`
records which inference variables stand for which type parameters, and
`finish_fn` the built-in bounds required of types. `show_value_type` then
shows a function type whose variables remain open as generic, named after
those parameters (or `T`, `U`, ...), with their bounds: `fn<T: Ord>([T]) ->
T?`. Type mismatches are shown with `Checker::show_pair`, which, when two
distinct type definitions with the same name occur, tags each with where it
is defined (`TypePrinter::show_tagged`): `Point (defined in input 3)`, or
`(defined in module geo)`, with the position if that is still ambiguous.

## Control flow: labels and `defer`

* **Labels.** The checker resolves `break`/`continue` (labeled or not) to a
  loop of the current function or lambda (`FnCtx::loops`) and records the
  target in the HIR as a depth (`HKind::Break { depth }`, 0 = innermost);
  the compiler indexes its own loop stack with it. Comprehension clauses get
  hidden loop contexts in the checker (the compiler compiles them as loops),
  so that depths agree and an unlabeled `break` skips them.
* **`defer`.** The checker produces `HKind::Defer(e)` as a statement of the
  enclosing `HKind::Block` and rejects `return`, `?` and `break`/`continue`
  to outer loops inside `e` (`FnCtx::in_defer`). The compiler keeps a stack
  of defer scopes per function (`FnState::defers`, one per block being
  compiled); a `Defer` statement pushes a clone of `e` on the innermost scope
  and emits nothing. Every exit edge inlines the deferred code it leaves, as
  Zig does: the normal end of a block (unless its type is `never`), `break`
  and `continue` (the scopes above the target loop's `LoopCx::defer_depth`),
  and `return`, the failure path of `?` and `TryTop` (all scopes). So `defer`
  costs nothing at runtime beyond the deferred code itself, and code that
  does not use it compiles exactly as before. Each copy gets fresh registers
  for the locals it declares (`local_reg` is restored after each copy), and
  lambdas in deferred code are compiled once per copy. Values leaving a
  block (its value, `break` values, return values, the operand of `?`) are
  computed into a fresh register before the deferred code runs, and calls
  are not compiled as tail calls while defers are pending. Copies cost
  defers × exits in the worst case, so a function's copies are capped
  (`MAX_DEFER_OPS`, counting lambdas compiled for them) and compilation fails
  beyond that instead of growing without bound.

## Iterators

The lazy iterators (spec §12.13) are written in Sand in the prelude: the trait
`Iterator<T>` and the struct `Iter<T> { next: fn() -> T? }`, whose adapters
wrap the source's `next` closure in a new one (capturing the closure itself,
not the `Iter`, to save a field load per element) and whose consumers are
loops, so every element costs calls and fuel and endless iterators cannot
run unboundedly inside a native.

The `next` functions of the iterators of lists (and so maps and sets),
ranges and strings (`chars_iter`) are `Obj::IterFn` objects: the state of
the iteration (`IterSrc`: the list, the next index and the length when the
iterator was made, ...) as a callable function value. The `Call`
instruction steps one directly (`Vm::iter_step`), with no frame and no
native call; it is still one unit of fuel per element.

The checker knows the trait and the struct (`Checker::iter_trait`,
`iter_struct`, found when the prelude's items are declared). `iter_plan`
decides how a `for` loop or comprehension clause iterates: ranges, lists,
maps, sets and channels keep their dedicated instructions (`HKind::For`);
anything else that implements `Iterator` is lowered to

```
{ let <tmp> = iterable; while let Some(pat) = <next> { body } }
```

where `<next>` calls the `next` closure field of an `Iter` directly
(`CallValue` of a `Field`), or is a `CallTrait` of `Iterator.next` with
evidence resolved as usual (a direct call for a concrete type, a dictionary
call for a bounded type parameter).

### Fusion

Iterating through the adapters costs a closure call per adapter and element
plus a `Some` per level. So when a whole pipeline is visible where it is
consumed, `src/check/fuse.rs` lowers it to one loop instead (stream fusion,
at the HIR level, while the enclosing function is being checked):

* **Pipelines**: a source — `[T].iter()` (a native `ForList` loop), a
  range's `iter()` (`For` over the range, `UnpackRange` + `ForRangeIncl`,
  no list), or any other `Iter` expression (its `next` called directly) —
  followed by the adapters `map`, `filter`, `filter_map`, `enumerate`,
  `inspect`, `take`, `skip`, `take_while`, `skip_while`. Others (`zip`,
  `chain`, `flat_map`, `step_by`) end a pipeline: what precedes them is an
  ordinary `Iter` source of the next one.
* **Consumers**: `for` loops and comprehension clauses (`iter_loop`), and
  calls of `to_list`, `fold`, `each`, `count`, `any`, `all`, `find` and
  `sum` (for `int`/`float` elements), hooked in `check_method_call`.
* **Eager methods** of lists (`map`, `filter`, `filter_map`, `take_while`,
  `count`, `fold`, `each`, `any`, `all`, `find`) and ranges (`map`,
  `filter`, `each`) are one-stage pipelines with a consumer (`xs.map(f)` is
  `xs.iter().map(f).to_list()`; the prelude implements them with exactly
  that loop), so they are lowered the same way.
* **Inlining**: a lambda literal passed to a fused adapter or consumer is
  called at exactly one place of the loop, so it is inlined there (its
  parameters become `let`s; its locals are re-owned by the enclosing
  function, so the compiler's capture analysis sees them as its own) unless
  its body contains `return` or `?`.

The lowering preserves evaluation order and the number of pulls: the source
and the adapters' arguments are evaluated once, in order, into hidden
locals; each adapter's function runs for the same elements in the same
order; `take` checks its count when a pull of it *starts* (at the top of the
loop, before the source is pulled, outermost `take` first), so it never
pulls past its count. A list source becomes a native list loop (whose length
snapshot is taken when the loop starts, where `[T].iter` takes it when
called) only if the adapters' and the consumer's arguments are inert
(lambdas, locals, literals). The loop is one HIR loop, so the depths of the
body's `break`/`continue` stay valid; adapters skip elements with
`continue` and stop with `break`. Fused, `xs.iter().map(f).filter(g)` over
1M elements takes about 10 ns per element (against 100 ns through the
closures, and 25 ns for the eager `xs.map(f).filter(g)`, which builds two
lists); an iterator stored in a variable and consumed later still goes
through the closures.

## Tests

* `cargo test --test spec_runner <file>` runs `tests/spec/<file>.test`;
  `SPEC_FILTER=substring` limits it to matching cases.
* API, sandbox, network and example tests live in `tests/*.rs`.
* `tests/cli.rs` runs the built `sand` binary. The prompt dialogue
  (`prompt_dialogue`, generic over the reader and writer) is unit-tested in
  `src/main.rs`; most CLI tests use the hidden `sand run --prompt-stdin`
  option, which asks the same questions on standard error and reads the
  answers from standard input (the script then gets no standard input).
* `tests/prompt_tty.rs` drives the real `--prompt` path on a pseudo-terminal
  (the `portable-pty` dev-dependency): on Unix a pty that becomes the child's
  controlling terminal, so `sand` opens it as `/dev/tty`; on Windows a ConPTY
  pseudo console, which `CONIN$`/`CONOUT$` open. The test waits for each
  question on the terminal, types the answer and checks the script's results
  and the terminal marker line (which `--prompt-stdin` does not print). It
  passes with a message if no pseudo-terminal can be opened. (The Windows
  variant has only been compiled, not run: CI has not run on Windows yet.)
* `tests/fuzz.rs` holds randomised robustness tests and, in
  `fuzz_regressions`, every reproducer found by the `cargo fuzz` targets in
  `fuzz/` (`cargo +nightly fuzz run compile|run|fmt`).
* GC stress: every spec file also runs under `Config::gc_stress` (the
  `spec_runner` tests in `mod gc_stress`), the CPU-bound examples are compared
  with and without it, and `tests/api.rs` has targeted write-barrier tests.
  Run any script under stress with `SAND_GC_STRESS=1 sand run file.sand`.
