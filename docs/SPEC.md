# The Sand Language Specification

Version 0.1

Sand is a small, statically typed, expression-oriented scripting language with a
sandboxed bytecode virtual machine written in safe Rust. It is designed to be
embedded: every effect a script can have on the outside world goes through a
host-defined policy, and every engine runs under host-defined resource limits.

This document is normative. Sections marked _(informative)_ are not.

---

## Contents

1. [Design goals](#1-design-goals-informative) _(informative)_
2. [Source text and lexical structure](#2-source-text-and-lexical-structure)
3. [Programs, modules and imports](#3-programs-modules-and-imports)
4. [Declarations](#4-declarations)
5. [Types](#5-types)
6. [Expressions](#6-expressions)
7. [Statements](#7-statements)
8. [Patterns](#8-patterns)
9. [Type checking and inference](#9-type-checking-and-inference)
10. [Runtime semantics](#10-runtime-semantics)
11. [Concurrency and asynchronous I/O](#11-concurrency-and-asynchronous-io)
12. [The prelude](#12-the-prelude)
13. [Standard library](#13-standard-library)
14. [Sandbox and policy](#14-sandbox-and-policy)
15. [Resource limits](#15-resource-limits)
16. [Embedding API (Rust)](#16-embedding-api-rust)
17. [Tooling](#17-tooling)
18. [Diagnostics](#18-diagnostics)

---

## 1. Design goals _(informative)_

- **Correct by default.** Immutable bindings, explicit mutation (`var` bindings and
  `var` fields), checked integer arithmetic, exhaustive pattern matching, no null,
  errors as values (`Result`, `Option`), and a linter that flags dubious code.
- **Well typed, lightly annotated.** Function signatures are annotated; everything
  inside a function body is inferred.
- **Elegant.** Few keywords, expression-oriented blocks, string interpolation,
  comprehensions, method chaining, named and default arguments, duration
  literals.
- **Fast without a JIT.** Static types let the compiler emit specialised
  instructions (integer add, float compare-and-branch, direct calls) for a
  register-based VM.
- **Sandboxed.** No ambient authority. Files, network, environment, standard
  streams and module imports are all mediated by a policy, which may ask the user
  interactively. There is no FFI, no subprocess, no IPC, no `mmap`.
- **Embeddable.** An `Engine` is `Send`, single-threaded internally, and many may
  run concurrently in one process.

---

## 2. Source text and lexical structure

### 2.1 Encoding

Source files are UTF-8. A leading byte-order mark is ignored. Source files use the
extension `.sand`. Non-ASCII characters may appear only in comments and string
literals.

### 2.2 Comments

```sand
// line comment
/* block comment, /* may nest */ */
/// documentation comment for the following item, field, variant or method
//! documentation comment for the enclosing module (only before the first item)
```

`help` shows doc comments (§12.15). A doc comment on an `extend` block
documents a built-in type.

### 2.3 Line termination

Newlines are significant. The lexer converts a newline into a statement
terminator (`;`) when the last token on the line is one of:

- an identifier, keyword `self`, `Self`, `true`, `false`, `break`, `continue`, `return`
- a literal (number, duration, string, bytes)
- a closing `)`, `]` or `}`
- the postfix operator `?`
- `..` or `..=`, which may end an open range (`let rest = 1..`)

and the newline is **not** inside a `(...)` or `[...]` group. Inside `{...}`
newlines are significant again, even when nested inside parentheses.

No terminator is inserted if the next non-blank line starts with `.`,
`else`, `and`, `or`, `=>`, a binary operator, or a `-` followed by whitespace
(see below). This permits chains such as:

```sand
let total = items
    .filter(item => item.price > 0 and item.in_stock)
    .map(item => item.price * item.quantity)
    .sum()
```

**Minus and whitespace.** `-` is the only operator that is both prefix and
binary; where the grammar cannot tell which is meant, whitespace decides. A
`-` with whitespace (space, tab, line break or start of source) before it and
none after it (`-x`) is a negation:

- at the start of a line it begins a new statement, whatever the previous
  line ends with (the linter reports it if it has no effect, `no-effect`,
  §17.2);
- after an operand (on the same line, or the next line inside `(...)` or
  `[...]`) it is an error (an ambiguous `-`): `a -b` and `[1` / `-2]` are
  rejected rather than read as `a - b` and `[1 - 2]`.

Every other `-` subtracts after an operand and negates elsewhere: `a - b`,
`a-b` and `a- b` subtract; `-a`, `f(-a)`, `[1, -2]`, `x = -1` and the second
`-` of `a - -b` negate. So a line starting with `-` and whitespace continues
the previous line:

```sand
let total = base
    - discount
```

The formatter prints subtraction with spaces on both sides and negation with
none after, so formatted code means what it says. Explicit `;` may separate
statements on one line; redundant terminators are ignored.

### 2.4 Identifiers and keywords

```text
ident = [A-Za-z_][A-Za-z0-9_]*
```

Reserved keywords:

```text
and as break continue defer else enum extend false fn for if impl import in
inf let loop match nan native not or pub return self Self spawn struct trait
true type use var while
```

`test` and `from` are contextual keywords. At the top level of a module, `test`
followed by a string literal starts a test block, and `from` followed by a
module name or path and `import` starts a selective import. In all other
positions, `test` and `from` are ordinary identifiers.

By convention (enforced by the linter): types, traits, enum variants and type
parameters are `UpperCamelCase`; functions, variables, fields, modules and loop
labels are `snake_case`; top-level `let` constants may also be
`SCREAMING_SNAKE_CASE`. The identifier `_` is a wildcard and may not be read.

### 2.5 Literals

**Integers**: `0`, `42`, `1_000_000`, `0xff`, `0b1010`, `0o777`. Underscores may
separate digits. A decimal literal must fit in a 64-bit signed integer (the
literal `9223372036854775808` is permitted only as the operand of unary minus).
A hexadecimal, binary or octal literal may be any 64-bit pattern, read as a
two's complement integer: `0x7FFF_FFFF_FFFF_FFFF` is the largest `int`,
`0x8000_0000_0000_0000` the smallest and `0xFFFF_FFFF_FFFF_FFFF` is `-1`.

**Floats**: `1.0`, `0.5`, `1e9`, `2.5e-3`, `1_000.5`, `inf` (positive
infinity), `nan` (not a number). A float must have digits on both sides of a
`.`; `1.` and `.5` are not floats.

**Durations**: an integer or float literal immediately followed by a unit:
`ns`, `us`, `ms`, `s`, `m`, `h`, `d` (24 hours), `w` (7 days). Examples: `250ms`,
`1.5s`, `2h`, `3d`. The value is
stored as a signed 64-bit number of nanoseconds, computed exactly from the
decimal literal (`1.005s` is 1,005,000,000 ns); fractional nanoseconds are
truncated.

**Booleans**: `true`, `false`.

**Strings**: `"..."`. Escapes: `\\`, `\"`, `\n`, `\r`, `\t`, `\0`, `\'`,
`\u{XXXX}` (1-6 hex digits, a Unicode scalar value), and `\(` which starts an
interpolation. Any other escape is an error, including `\{` and `\}`.
Strings may contain literal newlines; `{`, `}`, `(`, `)` and `:` are ordinary
text (`"[0-9]{4}"`, `"{\"a\": 1}"`).

**Interpolation**: inside a non-raw string, `\(expr)` inserts the display form
(§10.9) of `expr`; `\(expr:spec)` applies a format specification (§10.10). The
expression is lexed as ordinary code up to the `)` closing the `\(` (nested
parentheses do not end it) and may contain any tokens, including strings with
their own interpolations (`"\(xs.map(x => "<\(x)>"))"`). The spec begins at
the first `:` not nested inside `()`, `[]`, `{}` or a string and ends at the
next `)`, which ends the interpolation. A line break is allowed only inside
brackets opened within the interpolation; elsewhere, including in a spec, it
is an error (`unterminated interpolation`), reported at the `\(`. `\()` is an
error (`empty interpolation`).

**Multi-line strings**: `"""` starts a block string ending at the next `"""`
outside its interpolations; escapes and interpolation work as in `"..."`. A
newline right after the opening `"""` is removed, and the leading whitespace
common to the non-blank lines and the closing `"""` line (unless it is the
first line) is removed from every line. Lines that start inside an
interpolation are code: neither counted nor dedented.

**Raw strings**: `r"..."`, `r#"..."#`, `r##"..."##`, ... have no escapes and no
interpolation.

**Bytes**: `b"..."` produces a `bytes` value. Escapes as in strings plus
`\xNN`; no interpolation (`\(` is an invalid escape). Non-ASCII characters
are encoded as UTF-8.

### 2.6 Operators and punctuation

```text
+  -  *  /  %  **  &  |  ^  ~  <<  >>
== != <  <= >  >=  =  += -= *= /= %=
.. ..= => -> ?? ? . , : ; @ !
( ) [ ] { }
```

`!` occurs only in a macro call (§6.13). Negation is written `not`.

---

## 3. Programs, modules and imports

### 3.1 Modules

Every source file is a module. A module consists of an optional module doc
comment, `import` declarations, then any mix of item declarations (§4),
`test` blocks and top-level statements.

When a module is executed, its top-level statements and top-level `let`/`var`
initializers run in source order. Functions, types and methods may be referred
to before their textual definition. A top-level variable read before its
initializer has run fails (kind `uninitialized`).

The **main module** is the module the host asks the engine to run. Each imported
module is executed exactly once, before the first module that imports it,
following a depth-first order of `import` declarations. Import cycles are a
compile-time error.

A session (`sand repl`, §17.6) is a main module whose items arrive one input
at a time; each input is checked and run when it arrives, and may shadow the
names of earlier inputs.

### 3.2 Imports

```sand
import json                         // standard or host-native module, bound as `json`
import math as m                    // renamed
import "./geometry.sand"            // script module, bound as `geometry` (file stem)
import "lib/util.sand" as util      // script module, renamed
from math import cbrt, pi           // bind selected public names directly
from "./shapes.sand" import Shape, area
```

Script paths are resolved relative to the importing module's directory; the
`.sand` extension may be omitted, and other files cannot be imported. The
resolved path must lie within the **module root**: the main module's directory
unless the host configures another. A symlinked main file's imports and module
root are those of its target. Every script import is checked against the
policy as `Import(path)` (§14).

A module name (`json`) is resolved first to a host-registered native module,
then to a standard-library module, and then to `<library>/<name>.sand` if
`Config::library` is set. The engine checks a library module against the policy
as `Import(path)`, and the library directory can be outside the module root.
Only `pub` items are visible to importers; accessing a non-`pub` item or
importing a name that is not exported is an error. Qualified names are written
`module.name` and `module.Type`.

### 3.3 Visibility

`pub` may precede `fn`, `struct`, `enum`, `trait`, `type`, `let` and `var` at
the top level. Fields and methods of a public type are public, as are the
methods of a public trait. Impls have no visibility of their own (§4.9).
Top-level `var`s are exported read-only: importers may read but not assign
them.

---

## 4. Declarations

### 4.1 Functions

```sand
/// Returns the larger of two values.
pub fn max<T: Ord>(a: T, b: T) -> T {
    if a >= b { a } else { b }
}

fn greet(name: str, greeting: str = "Hello") {
    print("\(greeting), \(name)!")
}
```

- Every parameter has a type annotation. Parameters may have default values;
  defaults are evaluated at each call that omits them, in the callee's
  module scope, and may refer to earlier parameters. Parameters with defaults
  must follow all parameters without them.
- The return type follows `->`; if omitted it is `()` (unit).
- The body is a block whose value is returned. In a function returning `()`,
  the value of the final expression is discarded.
- Type parameters are listed in `<...>` with optional bounds `T: Ord + Hash`.
  Bounds are the built-in bounds `Eq`, `Ord`, `Hash` and `Num` (§5.6) and
  traits (§4.9), possibly module-qualified or with type arguments:
  `T: Ord + Describe`, `T: geo.Shape`, `T: Into<str>`. Bounds may also be
  given in a trailing `where` clause (`fn sum(self) -> T where T: Num`), which
  may also constrain the type parameters of an enclosing generic `extend`
  block, struct or enum for that one method.
- Functions may be declared inside blocks; such local functions may capture
  enclosing variables and may call themselves recursively. Local functions
  declared consecutively in the same block may call each other.

### 4.2 Structs

```sand
pub struct Point {
    x: float
    y: float

    /// Distance from the origin.
    fn norm(self) -> float {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    fn origin() -> Point { Point { x: 0.0, y: 0.0 } }
}

struct Counter {
    name: str
    var count: int = 0
}
```

- Fields are separated by newlines or commas and are immutable unless
  declared `var`. A field's default is used when a struct literal omits it.
- Methods are `fn` declarations inside the body. A method whose first parameter
  is `self` (no annotation) is an instance method, called as `value.method(...)`.
  Other methods are static, called as `Type.method(...)`.
- Within the body, `Self` refers to the struct type (with its type parameters).
- Structs may be generic: `struct Pair<A, B> { first: A, second: B }`.
- Structs have reference semantics: assigning a struct value shares it.

### 4.3 Enums

```sand
pub enum Shape {
    Circle(radius: float)
    Rect(w: float, h: float)
    Empty

    fn area(self) -> float {
        match self {
            Circle(r) => math.pi * r * r
            Rect(w, h) => w * h
            Empty => 0.0
        }
    }
}

enum Tree<T> {
    Leaf
    Node(left: Tree<T>, value: T, right: Tree<T>)
}
```

- Variants are separated by newlines or commas. A variant has zero or more
  payload fields; payload fields may be named (`radius: float`) or positional
  (`float`).
- Variants are referred to as `Enum.Variant` in expressions, e.g.
  `Shape.Circle(1.0)` or `Shape.Circle(radius: 1.0)`. In patterns the enum
  prefix may be omitted when the scrutinee type is known.
- `Option` and `Result` variants (`Some`, `None`, `Ok`, `Err`) are always in
  scope unqualified.
- Enum values are immutable.

### 4.4 Extending types

```sand
extend Point {
    fn scale(self, k: float) -> Point { Point { x: self.x * k, y: self.y * k } }
}
```

An `extend` block adds methods to a struct or enum declared in the **same
module**. Only the prelude and native module declarations may extend built-in
types (`extend str { ... }`, `extend<T> [T] { ... }`). Any module may add
methods to built-in types by implementing one of its own traits for them
(§4.9).

### 4.5 Type aliases

```sand
type Grid = [[bool]]
type Pair<T> = (T, T)
```

Aliases are transparent: `Grid` and `[[bool]]` are the same type. Aliases may
not be recursive.

### 4.6 Top-level variables

```sand
let GREETING = "hi"
pub let LIMITS = [1, 2, 3]
var counter = 0
```

Top-level `let` and `var` follow the same rules as local bindings; their types
must be fully inferred from their initializer and module-level uses.

### 4.7 Native declarations

In the prelude, standard library and host native modules, functions and
`impl` methods may be declared `native`, without a body; the module's registry
supplies the Rust implementation. User scripts may not declare `native`
functions.

```sand
pub native fn parse(text: str) -> Result<Json>
```

A native type parameter may be marked
`reflect` (`native fn decode<reflect T>(text: str) -> Result<T>`); the compiler
then passes a runtime description of the instantiated type, which must be fully
known at the call site.

`native type Name` declares an opaque type (§5.4).

### 4.8 Test blocks

```sand
test "addition is commutative" {
    assert(1 + 2 == 2 + 1)
}
```

The name is a string literal without interpolation. Test blocks are ignored by
`run` and executed by the test runner (§17.4). Inside a test block, the `test`
module (§13.14) and its public items are imported automatically, at lower
precedence than module definitions and the prelude. `?` propagates `None`
and `Err` as test failures (§6.8).

### 4.9 Traits and impls

A **trait** defines a set of methods that types can implement. Generic code
bounded by a trait can call its methods.

```sand
/// Things with a textual description.
pub trait Describe {
    fn describe(self) -> str                // required method
    /// A default method; it may call the trait's other methods.
    fn shout(self) -> str { self.describe().to_upper() }
}

pub trait Monoid {
    fn empty() -> Self                      // static method: no `self`
    fn combine(self, other: Self) -> Self
}

pub trait Shape: Describe + Eq {            // supertraits
    fn area(self) -> float
}

pub trait Into<T> {                         // a trait with a type parameter
    fn into(self) -> T
}

impl Describe for Point {
    fn describe(self) -> str { "(\(self.x), \(self.y))" }
}
impl<T: Describe> Describe for [T] {        // conditional (generic) impl
    fn describe(self) -> str { "[" + self.map(x => x.describe()).join(", ") + "]" }
}
impl Monoid for int {
    fn empty() -> int { 0 }
    fn combine(self, other: int) -> int { self + other }
}

fn concat_all<T: Monoid>(xs: [T]) -> T {
    xs.fold(T.empty(), (acc, x) => acc.combine(x))
}
fn show_all<T>(xs: [T]) -> str where T: Describe { xs.describe() }
```

**Trait declarations.**

- A trait body contains method declarations, optionally preceded by doc
  comments. A method without a body is **required**; a method with a body is
  a **default** method, used by impls that do not define it. Methods whose
  first parameter is `self` are instance methods; the others are static
  methods. Trait methods are not marked `pub` (they are public if the trait
  is) and cannot be `native`.
- Inside the trait, `Self` is a type parameter standing for the implementing
  type. It is bounded by the trait itself, so default methods may call any
  method of the trait (`self.describe()`, `Self.empty()`) and of its
  supertraits.
- A trait may have type parameters (`trait Into<T>`); they may not have
  bounds. Trait methods may have type parameters of their own, with bounds;
  `where` clauses in trait methods may only constrain those.
- Default values of trait method parameters must be constants (literals,
  `[]`, `[:]`, `None`).
- **Supertraits** follow a `:` and may be traits or built-in bounds:
  `trait Shape: Describe + Eq`. A bound `T: Shape` implies `T: Describe` and
  `T: Eq`. A trait may not be its own supertrait, directly or indirectly.
- Traits share the namespace of types. `Eq`, `Ord`, `Hash` and `Num` cannot
  be redefined.

**Impls.** `impl<P...> Trait<Args> for Type where ... { methods }`
implements a trait for a type.

- The type must have a type constructor: a struct, an enum (including
  `Option`, `Result`), `Error`, an opaque type, or a built-in type: `int`,
  `float`, `bool`, `str`, `bytes`, `duration`, `()`, `Range`, `[T]`,
  `[K: V]`, `Set<T>`, `Task<T>`, `Channel<T>` or a tuple (tuples of different
  lengths are different constructors). Function types and bare type
  parameters (`impl<T> Trait for T`) are not allowed. The type's arguments may
  be fixed (`impl Describe for [int]`).
- Each type parameter of the impl must occur in the type or in the trait
  arguments. The impl's bounds (in `<...>` or in a `where` clause after the
  type) are required wherever the impl is used: `impl<T: Describe>
Describe for [T]` makes `[T]` implement `Describe` exactly when `T` does.
- The impl defines each required method of the trait, may override default
  methods, and may not define other methods. Each method's signature must
  equal the trait's after replacing `Self` by the impl's type and the trait's
  type parameters by the impl's trait arguments (method type parameters may
  be renamed but must have the same bounds). Impl methods take no `pub`, no
  default values (those of the trait apply) and may not add bounds to the
  impl's type parameters. Within the impl, `Self` is the impl's type.
- **Supertraits:** the impl's type must implement every supertrait of the
  trait (possibly through conditional impls, under the impl's bounds) and
  satisfy its built-in supertraits.
- **Coherence:** a trait has at most one impl per type constructor and trait
  arguments. Two impls of a trait for the same constructor conflict when
  their trait arguments can be unified: `impl Into<str> for int` and
  `impl Into<float> for int` coexist, but `impl Describe for [int]` and
  `impl Describe for [str]` conflict.
- **Orphan rule:** an impl must be declared in the module that declares the
  trait or in the module that declares the type. Built-in types count as
  declared by the prelude, so other modules can implement only their own
  traits for them.
- Impls have no names and no visibility: once their module is loaded they
  apply throughout the program.

**Bounds.** A trait bound `T: Trait<Args>` is satisfied by a type with a
matching impl whose own bounds are satisfied (recursively: `[[Point]]:
Describe` needs `[Point]: Describe`, which needs `Point: Describe`), by a
type parameter bounded by the trait or by a trait that has it as a
supertrait, and by `Self` inside the trait. Bounds are checked at every
instantiation; an unsatisfied bound is reported as, for example, `the trait
Describe is not implemented for str (required for [str] to implement
Describe)`. Trait bounds may appear on functions, methods, `where` clauses
and impls, but not on the type parameters of structs, enums, type aliases,
traits, local functions or native functions.

**Method calls.** `e.m(args)` where the type of `e` is

- a type parameter: finds `m` among the traits of its bounds and their
  supertraits;
- any other type: after function-typed fields and inherent methods (§9),
  finds `m` among the traits that are public or declared in the current
  module and have an impl for the type's constructor. The trait does not
  need to be imported.

If several traits provide `m`, the call is ambiguous and an error. Every
trait method can also be called in qualified form:

```sand
Describe.describe(p)          // Self is inferred from the arguments
geo.Shape.area(c)             // a module-qualified trait
Point.describe(p)             // through the type
T.empty()                     // a static method through a bounded type parameter
Self.empty()                  // in traits and impls
let m: [str] = Monoid.empty() // Self is inferred from the expected type
```

Static methods cannot be called on a value. A call's trait arguments are
inferred from the bound, from the impl when the trait has a single impl for
the type's constructor, or from the expected type; when several impls remain
possible the call needs a type annotation.

Trait methods (`Point.describe`, `Describe.describe`, `T.empty`) and
generic functions with trait bounds can be used as function values; the
bounds are resolved where the value is created.

A trait is not a type: there are no trait objects (`dyn Trait`), so a list
holding values of different types that implement the same trait needs an
enum.

---

## 5. Types

### 5.1 Primitive types

| Type       | Values                                              |
| ---------- | --------------------------------------------------- |
| `bool`     | `true`, `false`                                     |
| `int`      | 64-bit signed integers                              |
| `float`    | IEEE-754 binary64                                   |
| `str`      | immutable UTF-8 text                                |
| `bytes`    | immutable byte strings                              |
| `duration` | signed 64-bit nanosecond spans                      |
| `()`       | unit, the single value `()`                         |
| `never`    | the type of expressions that do not produce a value |

`never` is a subtype of every type: an expression of type `never` (`return`,
`break`, `continue`, `fail(...)`) may appear where any type is expected.
Inside other types it is ordinary: `[never]` and `fn() -> never` are distinct
types, so `let xs: [never] = [1]` and passing `() => 5` where a
`fn() -> never` is expected are type errors.

### 5.2 Composite types

| Syntax          | Meaning                                                                                                            |
| --------------- | ------------------------------------------------------------------------------------------------------------------ |
| `[T]`           | growable list (reference type, mutable); shorthand for `List<T>`                                                   |
| `[K: V]`        | hash map, insertion ordered (reference type, mutable); shorthand for `Map<K, V>`                                   |
| `Set<T>`        | hash set, insertion ordered (reference type, mutable)                                                              |
| `(A, B, ...)`   | tuple of two or more elements (immutable); `(A,)` is an error, as are the pattern `(p,)` and the expression `(e,)` |
| `T?`            | shorthand for `Option<T>`                                                                                          |
| `Result<T, E>`  | `Ok(T)` or `Err(E)`; `Result<T>` means `Result<T, Error>`                                                          |
| `fn(A, B) -> R` | function; `fn(A)` returns `()`                                                                                     |
| `Task<T>`       | a concurrently running computation (§11)                                                                           |
| `Channel<T>`    | a message queue between tasks (§11)                                                                                |
| `Range`         | an integer range `a..b` / `a..=b`                                                                                  |

### 5.3 Nominal types

Structs and enums are nominal. Two struct types are equal only if they are the
same declaration with equal type arguments.

### 5.4 Built-in nominal types

The prelude defines:

```sand
pub enum Option<T> { Some(T), None }
pub enum Result<T, E = Error> { Ok(T), Err(E) }
pub struct Error { kind: str, message: str }
```

`Error.kind` is a short machine-readable tag. Standard kinds are:
`"error"`, `"not_found"`, `"permission_denied"`, `"already_exists"`, `"io"`,
`"timeout"`, `"parse"`, `"invalid"`, `"http"`, `"closed"`, `"limit"`.

The prelude also defines the lazy iterator struct `Iter<T>` and the trait
`Iterator<T>` (§12.13).

Standard library modules define further nominal types (e.g. `json.Json`,
`http.Response`, `net.Conn`). Some are _opaque_: they have no fields, cannot be
constructed with a literal, and are manipulated only by methods. If a module
registers an opaque type as a value type (§16, e.g. `time.PlainDate`), its
values are immutable, are compared and hashed by value, can implement `Ord`,
and are formatted as text (`2024-02-29`). Other opaque values are compared by
identity and formatted as `<Type>`.

### 5.5 Type equality and subtyping

Apart from `never`, there is no subtyping and there are no implicit conversions.
In particular `int` and `float` never convert implicitly: `1 + 2.0` is a type
error. Use `x.to_float()` or `f.to_int()`.

### 5.6 Bounds

| Bound  | Satisfied by                                                                                                                                                                       |
| ------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Eq`   | every type except function types (and types containing them)                                                                                                                       |
| `Ord`  | `int`, `float`, `bool`, `str`, `bytes`, `duration`, `()`, ordered value types (§5.4), and tuples, lists, `Option`, structs and enums all of whose components are `Ord`             |
| `Hash` | `int`, `bool`, `str`, `bytes`, `duration`, `()`, value types (§5.4), and tuples, `Option`, `Result`, enums and structs **without `var` fields** all of whose components are `Hash` |
| `Num`  | `int`, `float`                                                                                                                                                                     |

Map keys and set elements must be `Hash`. Built-in bounds are satisfied
structurally and cannot be implemented. Traits (§4.9) are user-defined bounds;
a trait whose supertraits include a built-in bound implies it.

A type parameter bounded by `Num` (directly, in a `where` clause, or through a
trait with `Num` as a supertrait) also has the static methods `T.zero()` and
`T.one()`, the additive and multiplicative identities (`0`/`1` for `int`,
`0.0`/`1.0` for `float`), so generic code can sum even an empty list:

```sand
fn total<T: Num>(xs: [T]) -> T {
    xs.fold(T.zero(), (a, b) => a + b)
}
let none: [float] = []
print(total(none))            // 0.0
```

---

## 6. Expressions

### 6.1 Precedence

From lowest to highest binding power:

| Level | Operators                                            | Associativity |
| ----- | ---------------------------------------------------- | ------------- |
| 1     | `??`                                                 | right         |
| 2     | `or`                                                 | left          |
| 3     | `and`                                                | left          |
| 4     | `not` (prefix)                                       | —             |
| 5     | `==` `!=` `<` `<=` `>` `>=`                          | none          |
| 6     | `\|`                                                 | left          |
| 7     | `^`                                                  | left          |
| 8     | `&`                                                  | left          |
| 9     | `<<` `>>`                                            | left          |
| 10    | `..` `..=`                                           | none          |
| 11    | `+` `-`                                              | left          |
| 12    | `*` `/` `%`                                          | left          |
| 13    | `-` `~` (prefix)                                     | —             |
| 14    | `**`                                                 | right         |
| 15    | postfix: call, `.field`, `.method()`, `[index]`, `?` | left          |

Comparison operators do not chain: `a < b < c` is a syntax error. `-2 ** 2` is
`-(2 ** 2)`.

### 6.2 Literals and names

Literals have their obvious types. A name refers to the innermost binding in
scope: local variable, parameter, local function, top-level item of the current
module, imported name, or prelude item.

### 6.3 Collections

```sand
[1, 2, 3]                 // [int]
[]                        // [T], T inferred
[0; 10]                   // ten zeros
[[0; w]; h]               // h distinct rows
["a": 1, "b": 2]          // [str: int]
[:]                       // empty map
[x * x for x in 0..10 if x % 2 == 0]       // list comprehension
[w: w.len() for w in words]                 // map comprehension
(1, "one")                // (int, str)
```

`[e; n]` evaluates `e` **once per element** (so nested lists are never
aliased). `n` must be an `int`; a negative `n` fails.

Comprehensions may have several `for` clauses and `if` filters, evaluated left
to right as nested loops: `[(x, y) for x in xs for y in ys if x != y]`.

### 6.4 Struct literals

```sand
Point { x: 1.0, y: 2.0 }
Point { x, y }                // field shorthand
Point { x: 5.0, ..p }         // copy remaining fields from p
geo.Point { x: 0.0, y: 0.0 }  // imported type
```

All fields without defaults must be given (or come from `..base`). A struct
literal is not permitted directly in the condition of `if`, `while`, `match`
or the iterable of `for`; parenthesise it instead.

### 6.5 Operators

**Arithmetic** `+ - * / % **` and unary `-` require both operands to have the
same `Num` type, with these additions:

- `str + str`, `bytes + bytes`, `[T] + [T]` concatenate (producing new values).
- When the operands' type is not known yet (`let add = (a, b) => a + b`), `+`
  requires that it be one of these (a number, `duration`, `str`, `bytes` or a
  list) once inferred: `add(true, false)` is a compile error.
- `duration + duration`, `duration - duration`, `-duration`,
  `duration * k`, `k * duration` and `duration / k` produce `duration`,
  where `k` is any `Num` (a type parameter `k: T` needs the bound `T: Num`);
  a `float` factor gives the exact product truncated toward zero to whole
  nanoseconds, and fails (kind `overflow`) if it is not finite or
  the product is out of range;
  a `float` divisor gives the exact quotient truncated toward zero to whole
  nanoseconds, is `0s` if the divisor is infinite, fails (kind `invalid`)
  if it is zero, and fails (kind `overflow`) if it is `nan` or the quotient
  is out of range;
  `duration / duration` produces `float`.

Integer semantics:

- Overflow in `+ - * **`, unary `-`, and `/` fails with `overflow: integer overflow`.
- `/` is **floor division** and `%` is **floor modulo**: `-7 / 2 == -4`,
  `-7 % 2 == 1`, `7 % -2 == -1`. Division or modulo by zero fails with
  `division by zero`.
- `a ** b` with `b < 0` fails with `invalid: negative exponent`.

Float semantics: IEEE-754 with round-to-nearest. `/` by zero yields `inf`,
`-inf` or `nan`. `%` is floor modulo (`a - b * floor(a / b)`). `**` is `powf`.

**Bitwise** `& | ^ << >> ~` operate on `int`. `>>` is arithmetic. A shift
amount outside `0..64` fails with `invalid: shift out of range`.

**Comparison** `== !=` require `Eq` operands of the same type and compare
structurally. `< <= > >=` require `Ord` operands of the same type. Ordering is:
numeric for numbers (NaN compares false with everything); lexicographic by
bytes for `str` and `bytes`; `false < true`; lexicographic for tuples and lists;
for enums, by variant declaration order then payload; for structs, field by
field in declaration order. Floats inside composites use the total order where
`nan` is greater than every other float and equal to itself.

Values of reference types (lists, maps, sets, structs) are compared **by
content**. Tasks, channels, and non-value opaque types (§5.4) are compared by
identity.

**Logical** `and`, `or`, `not` take and return `bool`; `and`/`or` short-circuit.

**Range** `a..b` (exclusive) and `a..=b` (inclusive) require `int` operands and
produce a `Range`.

**Coalesce** `a ?? b`: if `a: T?`, yields the payload of `Some` or else
evaluates `b: T`. If `a: Result<T, E>`, yields the `Ok` payload or else `b`.

### 6.6 Calls

```sand
f(1, 2)
f(1, scale: 2.0)              // named argument
point.norm()                  // method call
Point.origin()                // static method
Describe.describe(p)          // trait method in qualified form (§4.9)
http.get(url, timeout: 5s)    // module function with named argument
```

Positional arguments come first, then named arguments in any order. Named and
default arguments are available when the callee is statically known (a function
or method name); calls through function-typed values take positional arguments
only.

### 6.7 Field access, indexing and slicing

- `s.f` reads struct field `f`; `t.0`, `t.1` read tuple elements.
- `xs[i]` indexes a list (fails with kind `bounds` unless `0 <= i < len`).
- `m[k]` looks up a map key (fails with kind `not_found` if absent).
- `s[i]` on `str` and `bytes` is not permitted; use slices or methods.
- `xs[a..b]`, `xs[a..]`, `xs[..b]`, `xs[a..=b]` produce a new list. On `str`
  and `bytes`, slices produce new values and indices are **byte** offsets. A
  slice bound out of range, `a > b`, or a `str` bound that is not on a
  character boundary fails.

### 6.8 Option and error propagation

- Postfix `e?` where `e: T?` evaluates to the `Some` payload or returns `None`
  from the enclosing function, whose return type must be an `Option`.
- Postfix `e?` where `e: Result<T, E>` evaluates to the `Ok` payload or returns
  `Err(err)` from the enclosing function, whose return type must be
  `Result<_, E>` with the same `E`.
- At the top level of the main module or in a test block, `e?` unwraps `Some`
  and `Ok`. On `Err(err)`, `e?` fails execution with `err` (§10.3). On `None`,
  `e?` fails execution with an error of kind `none`. Using `?` at the top level
  of an imported module is an error.
- `?` is not permitted inside a lambda unless the lambda's return type is an
  appropriate `Option`/`Result`.

### 6.9 Blocks and control flow expressions

```sand
{ stmt; stmt; expr }
if cond { a } else if cond2 { b } else { c }
if let Some(x) = opt { x } else { 0 }
match value { pattern => expr, pattern if guard => expr }
loop { ... break value ... }
while cond { ... }
while let Some(line) = conn.read_line()? { ... }
for x in iterable { ... }
```

- A block's value is its final expression statement, or `()` if it ends with a
  non-expression statement or is empty.
- `if` without `else` has type `()`; its body's value is discarded. With `else`,
  both branches must have the same type, unless the `if` is used as a statement.
- `match` arms are separated by newlines or commas. Arms must be exhaustive
  (§8.3). All arm bodies must have the same type unless the `match` is used as a
  statement.
- `loop` evaluates to the value given to `break` (or `()` if `break` has no
  value). `while` and `for` evaluate to `()`; `break` in them takes no value.
- `for` (and a comprehension `for` clause) iterates over a `Range`
  (integers), `[T]` (elements), `[K: V]` (`(K, V)` tuples), `Set<T>`
  (elements), `Channel<T>` (messages received live until it is closed and
  empty), or any value whose type implements the prelude trait `Iterator<T>`
  (§12.13), such as `Iter<T>`, calling `next()` before each iteration until it
  returns `None`. The loop binding may be a pattern: `for (k, v) in map`. A
  list loop visits indices `0..n`, `n` being the length when the loop starts,
  reading each element when visited and stopping early if the list shrinks.
  Maps and sets are iterated over a snapshot taken when the loop starts.
- `break` and `continue` refer to the innermost loop. `return e` returns from
  the innermost function or lambda. At the top level of the main module,
  `return` ends the program (or the session input, §17.6) and may return a
  value of any type; returning `Err(err)` fails with `err` (§10.3). At the
  top level of an imported module it is an error. All three have type `never`.
- **Labels.** A loop may be labeled by writing `@name` before `loop`, `while`
  or `for`, on the same line; `break @name` and `continue @name` then refer to
  that loop from anywhere in its body, including nested loops:

  ```sand
  @rows for row in grid {
      for x in row {
          if x < 0 { break @rows }       // leaves both loops
          if x == 0 { continue @rows }   // next row
      }
  }
  let found = @search loop {
      for p in candidates() {
          if good(p) { break @search p }
      }
  }
  ```

  `break @name value` is allowed only when `@name` labels a `loop`;
  `continue @name` may target any loop (on `loop`, it restarts the body).
  Labels name loops of the current function or lambda only: `break` and
  `continue` cannot leave a lambda or local function. An unknown label, or one
  already used by an enclosing loop of the same function, is a compile error.
  Labels are not values. The label must directly follow the `@`.

### 6.10 Lambdas

```sand
x => x * 2
(a, b) => a + b
(acc: int, x: int) => acc + x
() => print("hi")
item => {
    let p = item.price
    p * 2
}
```

Parameter types are inferred from the expected function type, or from
annotations. Lambdas capture variables of enclosing scopes. Captured `let`
bindings are copied; captured `var` bindings are shared by reference, so
mutation inside the lambda is visible outside and vice versa.

### 6.11 String interpolation

`"\(a) + \(b) = \(a + b)"` builds a string from the display forms (§10.9) of the
embedded expressions, which may have any type. Lexical rules: §2.5.

### 6.12 `spawn`

`spawn e` evaluates expression `e` in a new task and returns a `Task<T>` where
`T` is the type of `e`. Variables are captured as for lambdas. See §11.

### 6.13 Macros

A macro call is written `name!(argument)`, always with parentheses. The only
macro is `json!`, which builds a `json.Json` value (§13.7) from JSON syntax
whose values can be Sand expressions:

```sand
let body = json!({
    "requests": [{"range": {"sheetId": id}, "fields": "bold"}],
    "values": rows,
    "note": null,
})
```

The argument is a **data value**, one of:

- an object, `{k: v, ...}` or `[k: v, ...]`, where each key `k` is an
  expression of type `str` and each value `v` is a data value. `{}` and `[:]`
  are empty objects;
- an array, `[v, ...]` of data values. `[]` is an empty array;
- `null`;
- any other expression `e`, converted with `json.to_json(e)`. A `Json` value
  is used as it is, and a value with no JSON form fails at run time.

`json!` expands to calls of the `json` module, which it loads without an
import; a binding named `json` does not change it. Newlines inside a data
value do not end statements, and a trailing comma is allowed. The formatter
writes every object with braces.

---

## 7. Statements

```sand
let x = 1                     // immutable binding
let y: float = 2.0            // annotated
var n = 0                     // mutable binding
let (a, b) = pair             // irrefutable pattern
let Some(v) = opt else { return }   // refutable pattern with diverging else
n = n + 1
n += 1                        // also -= *= /= %=
xs[i] = v                     // list element assignment
m[k] = v                      // map insert/overwrite
p.count += 1                  // `var` field assignment
expr                          // expression statement
defer conn.close()            // run when the enclosing block exits (§7.1)
use f = fs.open(path)?        // close `f` when the enclosing block exits (§7.2)
```

- Rebinding a name with `let` in the same block shadows it.
- Only `var` variables, `var` fields, list elements and map entries can be
  assigned. Assigning a `let` binding or non-`var` field is a compile error.
- `let ... else` requires the `else` block to have type `never`.
- An expression statement whose value has type `Result<_, _>` and is unused
  is reported by the linter (`unused-result`).

### 7.1 `defer`

`defer e` (usually `defer { ... }` or `defer x.close()`) schedules `e` to run
when the **enclosing block** exits, however it exits:

```sand
fn copy(src: str, host: str, port: int) -> Result<()> {
    let conn = net.connect(host, port)?
    defer conn.close()                  // runs on every exit below
    for line in fs.read_text(src)?.lines() {
        conn.write_str(line + "\n")?    // an error returns, closing conn
    }
    Ok(())
}
```

- **When.** The deferred expressions of a block run when control leaves the
  block: by falling off its end (after its value is computed), by `break` or
  `continue` (labeled or not) to a loop outside the block, by `return`, by
  `?` propagating an error or `None`, or through the diverging `else` of
  `let ... else`. A `defer` statement that was not reached does not run; a
  `defer` in a loop body runs at the end of each iteration that reached it.
  The blocks of function bodies, lambdas, `spawn` expressions, `test` blocks
  and the top level of a module are blocks too: top-level defers run when the
  module's top-level code finishes (or when a top-level `?` or `return`
  stops the program).
- **Order.** Defers run in reverse order of declaration (last first); when an
  exit leaves several blocks, the innermost block's defers run first.
- **Values.** A block's value, a `break` value and a `return` value are
  computed first, then the defers run, then control leaves: `return f()`
  calls `f()` before the defers (so it is not a tail call), and a defer that
  assigns a variable does not change a value already computed from it.
- **Variables.** The deferred expression sees the variables in scope at the
  `defer` statement and reads them when it runs (not when it is declared):
  `var s = 1; defer print(s); s = 2` prints `2`.
- **Restrictions.** The deferred expression must have type `()` (write
  `defer { let _ = f() }` to discard a value), and must not leave itself:
  `return`, `?`, and `break`/`continue` to a loop outside it are compile
  errors (loops, lambdas and `let ... else` inside it are fine). Deferred
  code is compiled into every exit of its block, so a function with very many
  defers and exits can exceed a compile-time limit (§15.3).
- **Errors and cancellation.** An error (§10.3) or `exit` aborts the run
  without running defers. A cancelled task (§11.2) runs its pending defers,
  innermost first; cancelling it again has no effect. Deferred code may
  suspend like any code.

### 7.2 `use`

A **resource** is a value whose type implements the prelude trait `Close`
(§12.14), such as an open file, a connection or an HTTP body. `use` binds one
and closes it when the enclosing block exits:

```sand
fn first_line(path: str) -> Result<str?> {
    use f = fs.open(path)?
    f.read_line()                       // f is closed after this, on every path
}
```

- `use name = e` (or `use name: T = e`) binds `e` immutably like `let` and
  closes it as `defer Close.close(name)` would (§7.1): on every exit, last
  declared first, interleaved with the block's defers. `use _ = e` keeps the
  value only to close it (`use _ = lock.acquire()`).
- The type of `e` must implement `Close` (a compile error otherwise); a
  `Result` of one needs `?` first. The trait's method is called even if the
  type also has an inherent `close`.
- A `use` binding is a name or `_`, not a pattern, and is local to its block;
  at a module's top level it is closed when the top-level code finishes. In a
  session (§17.6), a top-level `use` binds a session variable, as `let` does:
  later inputs may read it, and no input closes it (it is closed like a
  resource that no `use` closes, below).
- A copy of the value kept beyond the block (stored, returned, captured by a
  task) refers to a closed resource, whose operations fail with kind `"closed"`. The
  linter reports such escapes (`resource-escapes`, §17.2) and resources
  never closed (`unclosed-resource`).

A resource that no `use` closes is still closed at the latest when the
garbage collector finds it unreachable (§10.8).

---

## 8. Patterns

### 8.1 Syntax

```text
_                     wildcard
name                  binding
42  -1  "text"  true  literal (int, str, bool)
1..=9                 inclusive int range
(p1, p2)              tuple
Some(p)  None         Option variants
Ok(p)  Err(p)         Result variants
Circle(p)             enum variant (enum inferred from scrutinee)
Shape.Circle(p)       qualified variant
Point { x, y: 0 }     struct
Point { x, .. }
[]  [a, b]  [first, ..rest]  [.., last]   list patterns
p1 | p2               alternatives (must bind the same names with the same types)
```

A binding pattern `name` that coincides with a payload-less variant of the
scrutinee's enum type refers to that variant (`Empty` for a `Shape`, §4.3). A
capitalized name that is not such a variant is a binding, which the linter
reports as a likely mistyped variant (`variant-like-binding`, §17.2). Struct
patterns must either mention every field or end with `..`. A pattern binds a
name at most once.

### 8.2 Guards

`pattern if cond => body` matches only when `cond` is true. Guards do not count
towards exhaustiveness.

### 8.3 Exhaustiveness and reachability

A `match` must be exhaustive; otherwise it is a compile error listing a
missing pattern. Types with infinitely many values (`int`, `str`, lists, ...)
need a wildcard or binding arm (for lists, `[]` plus `[_, .._]` suffices). An
arm covered by earlier arms is a compile error (`unreachable pattern`). A
`match`, or refutability check of a `let`, `for` or comprehension pattern,
whose check exceeds a fixed amount of work (e.g. or-patterns in many tuple
positions) is rejected with `pattern too complex to check for exhaustiveness`
(§15.3).

---

## 9. Type checking and inference

- Each module is checked after the modules it imports.
- Function signatures (parameter and return types) are explicit. Within bodies,
  types are inferred by unification. Top-level functions and methods are
  generic over their declared type parameters; each use instantiates them
  freshly. Local `let` bindings and lambdas are monomorphic.
- Inference is bidirectional where it matters: arguments are checked against
  the instantiated parameter types, so lambda parameters get their types from
  the callee; the receiver of a method call and the subject of a field access
  must have a known type constructor at that point (otherwise: `type annotation
needed`).
- Method resolution: for `e.m(...)`, the type constructor of `e` determines the
  candidates, in order: a function-typed struct field `m` (whose value is
  called), an inherent method (declared in the type or an `extend` block), a
  trait method (§4.9; for a type parameter, from its bounds), and the
  universal methods `to_str() -> str` (display form) and `debug() -> str`
  (debug form).
- Integer and float literals have types `int` and `float` respectively; there
  is no literal polymorphism.
- At the end of checking a function, every type variable must be resolved,
  except those bound to the function's own type parameters. Unresolved
  variables are errors (`cannot infer type`).
- Bounds on type parameters are checked at instantiation; trait bounds
  (§4.9) once the function's types are inferred.
- A type, with inference variables resolved, has at most 10,000 components
  (type constructors and leaves); larger types, which only arise by repeated
  doubling (`let b = (a, a)`, or similar aliases or generic calls), are
  rejected with `type too large`. §15.3 lists the other compile-time limits.

---

## 10. Runtime semantics

### 10.1 Values

Values of type `int`, `float`, `bool`, `()`, `duration` and payload-less enum
variants are immediate. All other values live in the engine's garbage-collected
heap. Lists, maps, sets and structs have reference semantics; tuples, strings,
bytes and enum values are immutable, so sharing is unobservable.

### 10.2 Evaluation order

Operands, arguments, list elements, and fields are evaluated left to right.
Struct literal fields are evaluated in the order written; defaults are evaluated
afterwards in declaration order. For assignment `a[i] = e`, `a` and `i` are
evaluated before `e`; for compound assignment `a[i] += e`, `a` and `i` are
evaluated once.

### 10.3 Errors

A run **fails with an error** when the script cannot go on. The run ends (all
tasks), or in a session (§16.3, §17.6) the input does, with a runtime error:
`<kind>: <message>`, the display form of an `Error` (§5.4), and a stack trace
of `file:line` entries. Tools report it as `error: <kind>: <message>`, leaving
out a kind of `error`. A run fails with an error from:

- a top-level `?` on `Err(err)` (§6.8) or a top-level `return` of `Err(err)`
  (§6.9): the error `err`;
- `fail(message, kind)` (§12.1), whose kind defaults to `"error"`;
- faults detected while the script runs, with these kinds: `bounds` (an index,
  slice or byte out of range), `not_found` (a missing map key), `overflow`
  (integer or duration overflow), `invalid` (division by zero, a negative
  exponent, an argument out of its domain, a value JSON cannot encode),
  `limit` (stack overflow, nesting beyond 2,000 levels while printing,
  comparing or hashing a value, too many tasks), `unwrap` (`unwrap`/`expect`
  on `None`/`Err`), `assert` (a failed `assert`), `todo` (`todo()`),
  `cancelled` (waiting on a cancelled task), `deadlock` and `uninitialized`
  (a global used before initialization), and `permission_denied` (a denied
  `print`).

Scripts cannot catch errors, and failing does not run deferred expressions
(§7.1), except those of the top level for a top-level `?` or `return`; the
test runner isolates errors per test.

An **internal error** is a bug: an invariant of the engine broke, or a Rust
panic was caught in the engine or in a host native. It also ends the run, and
tools report it as `internal error: <message>`; a session in which one happens
is reset (§16.3).

### 10.4 Exit

`exit(code: int) -> never` stops the engine run immediately with the given exit
code. It does not terminate the host process.

### 10.5 Functions and closures

Calls are by value (references for reference types). Recursion depth is bounded
by the `max_call_depth` limit (default 10,000); exceeding it fails with
`stack overflow`. Tail calls (`return f(x)` or a call in tail position of the
function body) do not consume stack depth, except while deferred expressions
(§7.1) are pending, since they run after the call returns.

### 10.6 Maps and sets

Maps and sets iterate in insertion order. Re-inserting an existing key keeps its
position. Removing a key removes it from the order.

### 10.7 Strings

Strings are UTF-8. `len()` is the length in bytes. `chars()` yields Unicode
scalar values as one-character strings. Comparison is by bytes.

### 10.8 Garbage collection

Unreachable heap values are reclaimed by a tracing collector, observable only
through memory limits and the closing of unreachable resources (files,
sockets, HTTP bodies). So that dropped, unclosed resources never make an
operation fail, collections also run after every 64 resources opened and
before an operation would fail because the open-resource limit (§15.1) or the
system's open-file limit is reached.

### 10.9 Display and debug forms

`to_str()` (display form; used by `print` and interpolation) and `debug()`
(debug form; used inside composite values) are:

| Value                               | Display                             | Debug                                                                           |
| ----------------------------------- | ----------------------------------- | ------------------------------------------------------------------------------- |
| `42`                                | `42`                                | `42`                                                                            |
| `1.0`, `0.1`, `1e100`, `nan`, `inf` | `1.0`, `0.1`, `1e100`, `nan`, `inf` | same                                                                            |
| `true`                              | `true`                              | `true`                                                                          |
| `"hi"`                              | `hi`                                | `"hi"` (escaped with `\"`, `\\`, `\n`, `\r`, `\t`, `\u{..}` for other controls) |
| `b"a\x00"`                          | `b"a\x00"`                          | same                                                                            |
| `1500ms`                            | `1.5s`                              | same                                                                            |
| `()`                                | `()`                                | `()`                                                                            |
| `[1, 2]`                            | `[1, 2]`                            | same                                                                            |
| `["a": 1]`                          | `["a": 1]`                          | same                                                                            |
| `[:]`                               | `[:]`                               | same                                                                            |
| `Set` of 1, 2                       | `Set([1, 2])`                       | same                                                                            |
| `(1, "a")`                          | `(1, "a")`                          | same                                                                            |
| `Some(1)`                           | `Some(1)`                           | same                                                                            |
| `None`                              | `None`                              | same                                                                            |
| `Point { x: 1.0 }`                  | `Point { x: 1.0 }`                  | same                                                                            |
| `Circle(2.0)`                       | `Circle(2.0)`                       | same                                                                            |
| `Error`                             | `<kind>: <message>`                 | `Error { kind: "...", message: "..." }`                                         |
| function                            | `<fn name>`                         | same                                                                            |
| task / channel                      | `<task>` / `<channel>`              | same                                                                            |
| `0..3`                              | `0..3`                              | same                                                                            |
| `time.date(2024, 2, 29)?`           | `2024-02-29`                        | same                                                                            |
| other opaque value                  | `<Regex>`                           | same                                                                            |

Composite values print their components in debug form. The debug form of a
string is a string literal that evaluates to it. Durations display with
the largest unit among `d`, `h`, `m`, `s`, `ms`, `us`, `ns` that keeps the value
`>= 1`, using the shortest float representation of the value in that unit:
`90s` displays as `1.5m`, `36h` as `1.5d`, `1500ms` as `1.5s`, `0ns` as `0s`.

### 10.10 Format specifications

`\(expr:spec)` with `spec = [[fill]align][sign][0][width][.precision][type]`:

- `align`: `<` left, `>` right, `^` center. Default: right for numbers, left
  otherwise. `fill` is any single character other than `)` (default space).
- `sign`: `+` forces a sign for numbers.
- `0`: zero-pad numbers after the sign to `width`.
- `width`: minimum width in characters.
- `.precision`: digits after the decimal point for floats; maximum characters
  for strings.
- `type`: `x` / `X` hex, `b` binary, `o` octal (ints only); `e` scientific
  (floats); `?` debug form.

Examples: `"\(3.14159:.2)"` → `3.14`; `"\(42:>5)"` → `   42`; `"\(7:03)"` →
`007`; `"\(255:x)"` → `ff`; `"\("ab":*^6)"` → `**ab**`; `"\("hi":?)"` → `"hi"`.

---

## 11. Concurrency and asynchronous I/O

### 11.1 Model

An engine runs one task at a time on one OS thread. All I/O is asynchronous:
when a task performs I/O it is suspended and other ready tasks run. There is no
`async`/`await` coloring — any function may suspend. Tasks are scheduled
cooperatively with preemption at instruction-count boundaries, so a busy loop in
one task does not starve others.

When the main module's top-level code finishes, the run ends: unfinished tasks
are cancelled, and their pending defers run until they finish or wait for I/O,
a timer or another task. If every task is blocked and no I/O or timer is pending, the run
fails with `deadlock: all tasks are blocked`.

### 11.2 Tasks

```sand
let t = spawn fetch(url)          // Task<Result<Response>>
let r = t.wait()                  // suspends until done
t.is_done()                       // bool
t.cancel()                        // cancels if still running
```

- `spawn e` creates a task which starts running at the next scheduling point.
- `t.wait()` returns the task's result. Waiting on a cancelled task fails
  with `task cancelled`. Waiting on a task more than once returns the same
  value.
- Cancelling a task stops it at its next suspension point or instruction
  boundary and drops its pending I/O; it is done at once (waiting on it
  fails) and then runs its pending deferred expressions (§7.1). A deferred
  expression interrupted by the cancellation does not resume.
- An error in any task aborts the run (§10.3).

### 11.3 The `task` module (prelude-visible as `task`)

| Function                                           | Description                                                        |
| -------------------------------------------------- | ------------------------------------------------------------------ |
| `task.sleep(d: duration)`                          | suspend for `d`                                                    |
| `task.yield_now()`                                 | let other tasks run                                                |
| `task.all<T>(ts: [Task<T>]) -> [T]`                | wait for all, results in order                                     |
| `task.race<T>(ts: [Task<T>]) -> T`                 | wait for the first to finish; cancel the rest; fails on empty list |
| `task.timeout<T>(d: duration, f: fn() -> T) -> T?` | run `f` in a task; `None` (and cancel) if not done within `d`      |
| `task.channel<T>(capacity: int = 0) -> Channel<T>` | create a channel; `0` means unbounded                              |

### 11.4 Channels

| Method                   | Description                                                                  |
| ------------------------ | ---------------------------------------------------------------------------- |
| `ch.send(v: T) -> bool`  | enqueue; suspends while a bounded channel is full; returns `false` if closed |
| `ch.recv() -> T?`        | dequeue; suspends while empty; `None` once closed and empty                  |
| `ch.try_recv() -> T?`    | dequeue without suspending                                                   |
| `ch.close()`             | close; pending and future `recv` drain then get `None`                       |
| `ch.len() -> int`        | number of queued messages                                                    |
| `ch.is_closed() -> bool` |                                                                              |
| `ch.iter() -> Iter<T>`   | lazy iterator receiving messages until the channel is closed and empty       |

### 11.5 Prompts suspend everything

While the engine waits for an interactive policy decision (§14.3), no task
runs and no timers fire for scripts; the decision is applied before execution
resumes. (The host can still interrupt the run meanwhile, §15.1.)

---

## 12. The prelude

Always in scope without import.

### 12.1 Functions

| Function                                              | Description                               |
| ----------------------------------------------------- | ----------------------------------------- |
| `print<T>(x: T)`                                      | write display form + `"\n"` to stdout     |
| `eprint<T>(x: T)`                                     | write display form + `"\n"` to stderr     |
| `fail(message: str, kind: str = "error") -> never`    | fail with an error (§10.3)                |
| `assert(cond: bool, msg: str = "condition is false")` | fail with `msg` (kind `assert`) if false  |
| `todo() -> never`                                     | fail (kind `todo`): `not yet implemented` |
| `exit(code: int) -> never`                            | stop the run (§10.4)                      |
| `error(message: str, kind: str = "error") -> Error`   | construct an `Error`                      |
| `set<T: Hash>(items: [T] = []) -> Set<T>`             | build a set                               |
| `help(x)`, `help()`                                   | print documentation (§12.15)              |
| `members(x) -> [str]`, `members() -> [str]`           | list member names (§12.15)                |

### 12.2 Universal methods

Every value has `to_str() -> str` and `debug() -> str`.

### 12.3 `int`

`abs`, `pow(e: int) -> int`, `min(o)`, `max(o)`, `clamp(lo, hi)`, `to_float`,
`to_str_radix(radix: int) -> str` (2..36), `sign() -> int`, `is_even`, `is_odd`,
`to_char() -> str?` (Unicode scalar to one-char string). Static:
`int.max_value() -> int`, `int.min_value() -> int`. Parsing: `str.parse_int`
(§12.5).

### 12.4 `float`

`abs`, `floor`, `ceil`, `round` (half away from zero), `trunc`, `sqrt`, `pow(e:
float)`, `min`, `max`, `clamp`, `is_nan`, `is_finite`, `is_infinite`,
`to_int() -> int` (truncates; fails on NaN or out of range), `to_str_fixed(digits:
int) -> str`. `min` and `max` ignore a NaN argument. Parsing: `str.parse_float`
(§12.5).

### 12.5 `str`

| Method                                                                             | Description                                                                           |
| ---------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| `len() -> int`                                                                     | byte length                                                                           |
| `is_empty() -> bool`                                                               |                                                                                       |
| `chars() -> [str]`                                                                 | Unicode scalars                                                                       |
| `char_count() -> int`                                                              | number of scalars                                                                     |
| `codepoints() -> [int]`                                                            | scalar values                                                                         |
| `bytes() -> bytes`                                                                 | UTF-8 encoding                                                                        |
| `contains(sub: str) -> bool`                                                       |                                                                                       |
| `starts_with(p: str)`, `ends_with(s: str)`                                         |                                                                                       |
| `find(sub: str) -> int?`, `rfind(sub: str) -> int?`                                | byte offset                                                                           |
| `replace(from: str, to: str) -> str`                                               | all occurrences; an empty `from` returns the string unchanged                         |
| `split(sep: str) -> [str]`                                                         | `"".split(",") == [""]`; empty `sep` fails                                            |
| `split_once(sep: str) -> (str, str)?`                                              |                                                                                       |
| `split_whitespace() -> [str]`                                                      |                                                                                       |
| `lines() -> [str]`                                                                 | splits on `\n`, strips a trailing `\r` from each line, no final empty line            |
| `trim()`, `trim_start()`, `trim_end()`                                             | whitespace                                                                            |
| `strip_prefix(p: str) -> str?`, `strip_suffix(s: str) -> str?`                     |                                                                                       |
| `to_upper()`, `to_lower()`                                                         | Unicode-aware                                                                         |
| `repeat(n: int) -> str`                                                            |                                                                                       |
| `pad_start(width: int, fill: str = " ")`, `pad_end(...)`                           | pad to `width` characters                                                             |
| `parse_int(radix: int = 10) -> int?`, `parse_float() -> float?`                    | strict (no whitespace); radix 2..36; floats also `inf`, `-inf`, `nan`                 |
| `is_digit()`, `is_alpha()`, `is_alnum()`, `is_space()`, `is_upper()`, `is_lower()` | true if non-empty and all chars satisfy (ASCII classes for digit; Unicode for others) |
| `char_at(i: int) -> str?`                                                          | the scalar starting at byte offset `i`                                                |
| `ord() -> int`                                                                     | scalar value of a one-char string (fails otherwise)                                   |
| `reversed() -> str`                                                                | by scalars                                                                            |
| `chars_iter() -> Iter<str>`                                                        | lazy iterator over the scalars (§12.13)                                               |

### 12.6 `bytes`

`len`, `is_empty`, `get(i) -> int?`, `to_list() -> [int]`, `to_str() -> Result<str>`
(strict UTF-8; this shadows the universal `to_str`, use `debug()` for display),
`to_str_lossy() -> str`, `to_hex() -> str`, `contains(b: bytes)`, `find(b: bytes)
-> int?`, slicing. Static: `bytes.from_list(xs: [int]) -> bytes` (fails if any
element outside `0..=255`), `bytes.from_hex(s: str) -> bytes?`.

### 12.7 `duration`

`nanos() -> int`, `micros() -> int`, `millis() -> int`, `secs() -> float`,
`abs()`. Static: `duration.from_nanos(n: int)`, `duration.from_millis(n: int)`,
`duration.from_secs(s: float)`.

### 12.8 `[T]`

| Method                                                                          | Description                             |
| ------------------------------------------------------------------------------- | --------------------------------------- |
| `len()`, `is_empty()`                                                           |                                         |
| `push(x: T)`, `pop() -> T?`                                                     | at the end                              |
| `insert(i: int, x: T)`, `remove(i: int) -> T`                                   | fail if out of range                    |
| `extend(xs: [T])`, `clear()`                                                    |                                         |
| `get(i: int) -> T?`, `first() -> T?`, `last() -> T?`                            |                                         |
| `contains(x: T) -> bool`, `index_of(x: T) -> int?`                              | `T: Eq`                                 |
| `reverse()` (in place), `reversed() -> [T]`                                     |                                         |
| `sort()`, `sorted() -> [T]`                                                     | `T: Ord`, stable                        |
| `sort_by<K: Ord>(key: fn(T) -> K)`, `sorted_by<K: Ord>(key: fn(T) -> K) -> [T]` | stable                                  |
| `map<U>(f: fn(T) -> U) -> [U]`                                                  |                                         |
| `filter(f: fn(T) -> bool) -> [T]`                                               |                                         |
| `filter_map<U>(f: fn(T) -> U?) -> [U]`                                          |                                         |
| `flat_map<U>(f: fn(T) -> [U]) -> [U]`                                           |                                         |
| `each(f: fn(T))`                                                                |                                         |
| `fold<A>(init: A, f: fn(A, T) -> A) -> A`                                       |                                         |
| `reduce(f: fn(T, T) -> T) -> T?`                                                |                                         |
| `any(f: fn(T) -> bool)`, `all(f: fn(T) -> bool)`                                |                                         |
| `find(f: fn(T) -> bool) -> T?`, `find_index(f: fn(T) -> bool) -> int?`          |                                         |
| `count(f: fn(T) -> bool) -> int`                                                |                                         |
| `sum() -> T`, `product() -> T`                                                  | `T: Num`; empty: `T.zero()` / `T.one()` |
| `min() -> T?`, `max() -> T?`                                                    | `T: Ord`                                |
| `min_by<K: Ord>(key: fn(T) -> K) -> T?`, `max_by<K: Ord>(...)`                  | first minimal / first maximal           |
| `enumerate() -> [(int, T)]`                                                     |                                         |
| `zip<U>(other: [U]) -> [(T, U)]`                                                | truncates to the shorter                |
| `take(n: int)`, `skip(n: int)`                                                  | clamp to length                         |
| `take_while(f)`, `skip_while(f)`                                                |                                         |
| `chunks(n: int) -> [[T]]`, `windows(n: int) -> [[T]]`                           | `n > 0`                                 |
| `dedup() -> [T]`                                                                | remove consecutive duplicates           |
| `unique() -> [T]`                                                               | first occurrences, `T: Hash`            |
| `group_by<K: Hash>(key: fn(T) -> K) -> [K: [T]]`                                |                                         |
| `partition(f: fn(T) -> bool) -> ([T], [T])`                                     |                                         |
| `copy() -> [T]`                                                                 | shallow copy                            |
| `swap(i: int, j: int)`                                                          |                                         |
| `join(sep: str = "") -> str`                                                    | `T = str` only                          |
| `to_set() -> Set<T>`                                                            | `T: Hash`                               |
| `iter() -> Iter<T>`                                                             | lazy iterator (§12.13)                  |

Methods that take a function iterate over the initial list length in the same
way as a `for` loop (§6.9). Elements appended by the function during iteration
are excluded.

### 12.9 `[K: V]`

`len`, `is_empty`, `get(k) -> V?`, `get_or(k, default: V) -> V`,
`contains(k) -> bool`, `insert(k, v) -> V?` (returns old value), `remove(k) -> V?`,
`keys() -> [K]`, `values() -> [V]`, `entries() -> [(K, V)]`, `clear()`,
`copy()`, `map_values<W>(f: fn(V) -> W) -> [K: W]`, `filter(f: fn(K, V) -> bool)
-> [K: V]`, `extend(other: [K: V])`, `get_or_insert(k, default: V) -> V` (inserts
when absent and returns the stored value), `iter() -> Iter<(K, V)>` (§12.13).

### 12.10 `Set<T>`

`len`, `is_empty`, `add(x) -> bool` (true if newly added), `remove(x) -> bool`,
`contains(x)`, `to_list() -> [T]`, `union(o)`, `intersection(o)`,
`difference(o)`, `is_subset(o)`, `copy()`, `clear()`, `iter() -> Iter<T>`
(§12.13).

### 12.11 `Option<T>` and `Result<T, E>`

Option: `is_some`, `is_none`, `unwrap() -> T` (fails with `unwrap: unwrap on None`),
`expect(msg: str) -> T`, `unwrap_or(d: T) -> T`, `map<U>(f) -> U?`,
`and_then<U>(f: fn(T) -> U?) -> U?`, `or_else(f: fn() -> T?) -> T?`,
`ok_or<E>(e: E) -> Result<T, E>`, `filter(f: fn(T) -> bool) -> T?`.

Result: `is_ok`, `is_err`, `unwrap() -> T` (fails with `unwrap: unwrap on Err: <err>`),
`expect(msg: str) -> T`, `unwrap_or(d: T) -> T`, `unwrap_err() -> E`,
`map<U>(f) -> Result<U, E>`, `map_err<F>(f: fn(E) -> F) -> Result<T, F>`,
`and_then<U>(f: fn(T) -> Result<U, E>) -> Result<U, E>`, `ok() -> T?`,
`err() -> E?`.

### 12.12 `Range`

Range methods allocate a list only if the return type is `[int]`.

- `start() -> int`, `end() -> int` (exclusive end bound), `len() -> int`,
  `is_empty() -> bool`, `contains(i: int) -> bool`. `end` and `len` fail with
  `overflow` if the result exceeds `int::MAX`.
- `first() -> int?`, `last() -> int?`, `min() -> int?`, `max() -> int?`:
  return `None` if the range is empty.
- `sum() -> int`, `product() -> int`: compute the exact result (`0` or `1` if
  empty) in `O(1)` time, or fail with `overflow` if the result is out of
  `int` bounds.
- Function-taking methods (§12.8) for `int` elements: `map`, `filter`,
  `filter_map`, `flat_map`, `each`, `fold`, `reduce`, `any`, `all`, `find`,
  `find_index` (0-based offset from `start`), `count`, `take_while`,
  `skip_while`, `partition`, `group_by`, `min_by`, `max_by`.
- `to_list() -> [int]`, `reversed() -> [int]`, `step_by(n: int) -> [int]`
  (every `n`-th integer from the start; fails if `n <= 0`), `iter() ->
Iter<int>` (§12.13).

### 12.13 Iterators

The methods of lists, maps and ranges above are **eager**: `xs.map(f)` calls
`f` on every element at once and builds a new list. **Lazy iterators** do the
work only when elements are requested, one at a time, so pipelines build no
intermediate lists, can stop early, and can draw from endless sources:

```sand
let firsts = Iter.count_from(1).map(n => n * n).filter(n => n % 3 == 0).take(3)
print(firsts.to_list())                       // [9, 36, 81]
for (i, line) in text.lines().iter().enumerate().skip(1) { ... }
```

**The trait.** The prelude declares

```sand
pub trait Iterator<T> {
    fn next(self) -> T?                 // the next element; None once exhausted
    fn iter(self) -> Iter<T> { ... }    // a lazy Iter over the remaining elements
}
```

`for` loops and comprehensions iterate over any value whose type implements
`Iterator<T>` (§6.9). User types implement it like any trait (§4.9):

```sand
struct Countdown { var n: int }
impl Iterator<int> for Countdown {
    fn next(self) -> int? {
        if self.n <= 0 { return None }
        self.n -= 1
        Some(self.n + 1)
    }
}
for x in (Countdown { n: 3 }) { print(x) }          // 3, 2, 1
print(Countdown { n: 3 }.iter().map(x => x * 2).to_list())  // [6, 4, 2]
fn total<I: Iterator<int>>(it: I) -> int { it.iter().sum() }
```

**The `Iter<T>` struct** is the prelude's lazy iterator: `pub struct Iter<T>
{ next: fn() -> T? }`, a function producing the next element, which
implements `Iterator<T>` (its `iter()` returns the iterator itself). It is
obtained with `iter()` from lists, maps (`(key, value)` pairs), sets, ranges,
channels and any `Iterator`, with `chars_iter()` from strings, or from these
static sources:

| Source                                                    | Elements                                                             |
| --------------------------------------------------------- | -------------------------------------------------------------------- |
| `Iter.from_fn(f: fn() -> T?)`                             | the results of `f` until it returns `None` (also `Iter { next: f }`) |
| `Iter.empty()`                                            | none                                                                 |
| `Iter.once(x: T)`                                         | `x`                                                                  |
| `Iter.repeat(x: T)`                                       | `x` forever                                                          |
| `Iter.count_from(start: int, step: int = 1) -> Iter<int>` | `start`, `start + step`, ... forever                                 |
| `Iter.successors(first: T?, f: fn(T) -> T?)`              | `first`, then `f` of the previous element until `None`               |

**Adapters** return a new `Iter` and do no work until it is consumed:
`map<U>(f: fn(T) -> U)`, `filter(f: fn(T) -> bool)`, `filter_map<U>(f: fn(T) ->
U?)`, `flat_map<U>(f: fn(T) -> Iter<U>)`, `enumerate() -> Iter<(int, T)>`,
`zip<U>(other: Iter<U>) -> Iter<(T, U)>` (ends with the shorter), `chain(other:
Iter<T>)`, `take(n: int)` (pulls at most `n` elements from its source),
`skip(n: int)`, `take_while(f)`, `skip_while(f)`, `step_by(n: int)` (the first
element, then every `n`-th; fails if `n <= 0`), `inspect(f: fn(T))` (calls `f`
on each element as it passes).

**Consumers** pull elements: `to_list() -> [T]`, `to_set() -> Set<T>` (`T:
Hash`), `to_map() -> [K: V]` (for `Iter<(K, V)>`, `K: Hash`; a repeated key
keeps its last value), `join(sep: str = "") -> str` (for `Iter<str>`),
`each(f: fn(T))`, `fold<A>(init: A, f: fn(A, T) -> A) -> A`, `reduce(f: fn(T, T)
-> T) -> T?`, `sum() -> T` and `product() -> T` (`T: Num`; `T.zero()` /
`T.one()` when empty), `count() -> int` (the number of remaining elements), `any(f)`
and `all(f)` (stop at the first decisive element), `find(f) -> T?`,
`find_index(f) -> int?`, `first() -> T?` (the next element), `last() -> T?`,
`nth(n: int) -> T?` (the element `n` positions on, from 0), `min()`/`max() ->
T?` (`T: Ord`; the first minimal/maximal), `min_by`/`max_by<K: Ord>(key: fn(T)
-> K) -> T?`. A `for` loop over an `Iter` is a consumer too.

**Semantics.**

- **Laziness.** Creating an iterator or applying an adapter calls no
  function; each element is produced when a consumer asks for it, passing
  through the whole pipeline before the next one is requested. Adapters
  request only what they need: `Iter.repeat(1).take(3).sum()` terminates.
- **Single pass.** An iterator is a reference to its state: elements taken
  by any consumer (or by `next()` on any copy of the iterator, including one
  wrapped by an adapter) are gone. Consuming an exhausted iterator yields no
  elements.
- **Exhaustion.** List, range, map, set, string, and channel iterators
  return `None` after exhaustion; `Iter.from_fn` iterators call
  their function on each request. Adapters stop pulling from their source as
  soon as it returns `None`.
- **Mutation.** A list iterator behaves like a `for` loop (§6.9), with the
  length taken when `iter()` is called. Map and set iterators work on a
  snapshot of the entries taken by `iter()`, unaffected by later changes. A
  channel iterator receives live, suspending while the channel is empty.
- **Limits.** Iterators are ordinary Sand code: every element costs at least
  one unit of fuel (§15.2), so endless iterators are bounded by the
  instruction, time and memory limits (§15) like any loop.
- **Cost** _(informative)_. A pipeline written as one expression where it is
  consumed (`for x in xs.iter().map(f).filter(g) { ... }`,
  `xs.iter().map(f).sum()`), like an eager method taking a function
  (`xs.map(f)`), compiles to a single loop costing about as much as a
  hand-written one. An iterator stored and consumed later costs about one
  closure call per adapter and element.

### 12.14 `Close`

```sand
pub trait Close {
    fn close(self)
}
impl<T: Close> Close for T?        // closes the value, if any
impl<T: Close> Close for [T]       // closes the elements, last first
```

Resources (§7.2) implement `Close`: `fs.File`, `net.Conn`, `net.Listener`,
`http.Body`, `http.Stream`, `http.Upload`, `http.Reply`, `http.Server` and
`http.Request`. Closing a
resource again does nothing; its other operations fail with kind
`"closed"` once it is closed. A struct holding a resource should implement
`Close` by closing it (lint `resource-field`, §17.2).

### 12.15 `help` and `members`

The checker evaluates calls of `help` and `members`, whose argument may
be what no other function takes:

- a module, imported or not (`help(json)`);
- a type, trait or type parameter (`help(str)`, `help([int])`,
  `help(json.Json)`, `help(Close)`, `help(T)`);
- a function, method, trait method, field, variant or variable
  (`help(print)`, `help(str.split)`, `help(xs.map)`, `help(p.x)`,
  `help(Some)`, `help(math.pi)`, `help(xs)`);
- a string literal that identifies any of those targets (`help("json.Json.get")`);
  its first segment may be written as a type (`"List.map"`, `"[T].map"`,
  `"Set<T>.add"`, `"int?.unwrap"`);
- any other expression, which stands for its type and is not evaluated
  (`help(f())`, `help([1, 2])`).

`help(x)` prints the documentation of `x` to standard output, as `print`
would: its declaration and doc comment (for a variable, its declared or
inferred type), and for a module its items, for a type its variants,
fields, methods and the traits it implements, for a trait its methods and
implementing types, for any other expression its type and that type's
documentation. Lists of items show the first sentence of each
doc comment. `help()` prints an overview of the prelude and the modules.

`members(x)` returns the names that may follow `x.`, sorted: a module's
public items, or the variants, fields and methods (including those of
visible trait impls, `to_str` and `debug`) of a type or of an
expression's type. `members()` returns the names in scope, excluding the
prelude's.

Neither can be used as a function value. Defining `help` or `members`
shadows them as usual.

---

## 13. Standard library

Each module is imported with `import <name>`. I/O functions return `Result`
values with the `Error` kinds of §5.4 and are subject to the policy (§14);
unless stated otherwise, a denial is `Err(Error { kind: "permission_denied", ... })`.

### 13.1 `base64`

`encode(data: bytes) -> str`, `decode(text: str) -> Result<bytes>` (standard
alphabet, padding required), `encode_url(data: bytes) -> str`,
`decode_url(text: str) -> Result<bytes>` (URL-safe alphabet, no padding;
padding accepted on decode), `encode_str(text: str) -> str`.

### 13.2 `crypto`

`sha256(data: bytes) -> bytes`, `sha1(data: bytes) -> bytes`,
`hmac_sha256(key: bytes, data: bytes) -> bytes`, `md5(data: bytes) -> bytes`,
`constant_time_eq(a: bytes, b: bytes) -> bool`.

### 13.3 `fs`

Paths are strings. Relative paths are resolved against the engine's working
directory (set by the host; default: the module root).

| Function                                                                         | Capability                        |
| -------------------------------------------------------------------------------- | --------------------------------- |
| `read_text(path) -> Result<str>`                                                 | `FsRead`                          |
| `read_bytes(path) -> Result<bytes>`                                              | `FsRead`                          |
| `write_text(path, text: str) -> Result<()>`                                      | `FsWrite`                         |
| `write_bytes(path, data: bytes) -> Result<()>`                                   | `FsWrite`                         |
| `exists(path) -> Result<bool>`                                                   | `FsRead`                          |
| `is_file(path)`, `is_dir(path) -> Result<bool>`                                  | `FsRead`                          |
| `list_dir(path) -> Result<[str]>` (sorted names)                                 | `FsRead`                          |
| `create_dir(path, recursive: bool = false) -> Result<()>`                        | `FsWrite`                         |
| `remove_file(path)`, `remove_dir(path, recursive: bool = false)`                 | `FsWrite` (entry)                 |
| `rename(from, to) -> Result<()>`                                                 | `FsWrite` on both (entries)       |
| `copy(from, to) -> Result<()>`                                                   | `FsRead` on from, `FsWrite` on to |
| `metadata(path) -> Result<Metadata>`                                             | `FsRead`                          |
| `open(path, mode: Mode = Mode.Read) -> Result<File>`                             | by mode, below                    |
| `join(parts: [str]) -> str`, `basename(p)`, `dirname(p)`, `extension(p) -> str?` | none                              |

`struct Metadata { size: int, is_file: bool, is_dir: bool, modified: time.Instant? }`.

`/` is a separator on every platform (on Windows `\` is too). `join` inserts
the platform's separator, and a part that is an absolute path replaces the
parts before it; `basename`, `dirname` and `extension` are lexical. A path
longer than 4096 bytes or containing NUL fails with kind `"invalid"` before
anything else is done. Capabilities and prompts report absolute, normalized paths (§14.1).
Permission errors report the path (`permission denied: read data/x.txt`),
revealing neither symlink targets nor the on-disk names of inaccessible entries.

Reading, writing and copying accept regular files only (kind `"invalid"`
otherwise, e.g. for directories and FIFOs). `copy` copies the contents
only: the new file gets default permissions, never the source's execute or
setuid bits. `create_dir(path, recursive: true)` checks `FsWrite` for every
directory it creates, and creates none that existed when it checked.

Symbolic links (§14.1):

- Most functions follow symlinks: the policy is checked for the resolved
  location (the link's target), which the operation then accesses
  **without following any symlink**, in the directory the resolution
  reached, even if that directory is moved or replaced by a symlink
  meanwhile. If a symlink replaces the final entry between the check and
  the operation, or the path leads through a dangling symlink (one whose
  target, or a directory after it, did not exist when the path was
  resolved), the operation fails with kind `"permission_denied"`; `exists`,
  `is_file` and `is_dir` return `false` instead.
- `remove_file`, `remove_dir` and `rename` act on the entry itself: a final
  symlink is removed or renamed, never its target, and the capability
  ("entry") is checked for the link's own path (its directory resolved).
- `rename` fails with `"permission_denied"` if a deny rule decides an entry
  of the tree it moves or replaces, at either the entry's old path or its
  new one. `remove_dir(path, recursive: true)` makes the same check before
  removing anything, and also checks each entry before removing it.

On Windows, a junction counts as a symlink, and names that Win32 would
alter or treat specially (ending in `.` or a space, containing `:`, or a
reserved device name such as `CON` or `nul.txt`) fail with kind `"invalid"`.

A hard link to an outside file, placed in an allowed directory by someone
else, is the same file and may be accessed; the engine cannot in general
tell it from an ordinary file. Do not allow access to directories that
untrusted parties can write hard links into.

**Open files.** `open` opens a regular file for incremental reading and
writing, returning a `File` (a resource, §7.2):

```sand
pub enum Mode { Read, Write, Append, CreateNew, ReadWrite }
use f = fs.open("data.csv")?
while let Some(line) = f.read_line()? { ... }
```

| Mode        | Opens                                                                 | Capability          |
| ----------- | --------------------------------------------------------------------- | ------------------- |
| `Read`      | an existing file, for reading                                         | `FsRead`            |
| `Write`     | for writing from the start, creating the file or truncating it        | `FsWrite`           |
| `Append`    | for writing at the end (every write), creating the file if needed     | `FsWrite`           |
| `CreateNew` | a new file for writing; kind `"already_exists"` if something is there | `FsWrite`           |
| `ReadWrite` | an existing file, for reading and writing in place                    | `FsRead`, `FsWrite` |

The policy is checked **once**, at `open`. The `File` then stays the file
that was checked: renaming or removing its path, or putting a symlink in
its place, does not affect it (a removed file stays readable and writable
until closed, and in a memory file system counts against its capacity until
then). It counts against `max_open_resources` until closed or collected.

`File` implements `io.Reader`, `io.Writer` (§13.5) and `Close`. Reads and
writes move its position (`Append` writes go to the end); reads are
buffered (16 KiB, counted against `max_memory`), writes are not. `read(max)`
returns at most 1 MiB; `read_line` fails with kind `"limit"` (dropping what
it read) if the line does not fit in memory, and `read_all`, before
reading, if the rest of the file does not. Methods: `seek(pos: int)` (from
the start; past the end is allowed, and writing there fills the gap with
zeros), `position() -> Result<int>`, `metadata() -> Result<Metadata>`,
`set_len(len: int)` (truncates or zero-extends; not in `Read` mode),
`sync() -> Result<()>` (waits until written data is durable), `path() ->
str` (as given to `open`). Reading a file not opened for reading, or
writing one not opened for writing, fails with kind `"invalid"`; any
operation on a closed file with kind `"closed"`.

**Memory file systems.** An engine may give scripts a memory file system
instead of the host's (§16.1; always without the `os` feature). Paths are
absolute from its root `/` (the default working directory and module root)
and normalized lexically. It has no symlinks, hard links or permissions,
names are case sensitive everywhere, and a write that would exceed its
capacity fails with kind `"limit"`. Operations complete at once rather than
off the VM thread.

### 13.4 `http`

`http` and `net` (§13.10) need operating-system access: without the `os`
feature (§16.1) they do not exist, and importing them is a compile error
saying so.

Headers are lists of `(name, value)` pairs, in order; a header sent several
times (such as `Set-Cookie`) occurs several times. Names of received
headers are lower-cased.

```sand
pub struct Response { status: int, headers: [(str, str)], body: bytes, url: str }
pub fn get(url: str, headers: [(str, str)] = [], timeout: duration = 30s) -> Result<Response>
pub fn request(method: str, url: str, headers: [(str, str)] = [], body: bytes = b"",
               timeout: duration = 30s, follow_redirects: bool = true) -> Result<Response>
pub fn post(url: str, body: bytes, headers: [(str, str)] = [], timeout: duration = 30s) -> Result<Response>
pub fn post_json<T>(url: str, value: T, headers: [(str, str)] = [], timeout: duration = 30s) -> Result<Response>
pub fn put_json<T>(url: str, value: T, headers: [(str, str)] = [], timeout: duration = 30s) -> Result<Response>
pub fn patch_json<T>(url: str, value: T, headers: [(str, str)] = [], timeout: duration = 30s) -> Result<Response>
pub fn request_json<T>(method: str, url: str, value: T, headers: [(str, str)] = [], timeout: duration = 30s,
                       follow_redirects: bool = true) -> Result<Response>
pub fn open(method: str, url: str, headers: [(str, str)] = [], body: bytes = b"",
            timeout: duration = 30s, follow_redirects: bool = true) -> Result<Stream>
pub struct Stream { status: int, headers: [(str, str)], body: Body, url: str }
pub fn upload(method: str, url: str, headers: [(str, str)] = [], timeout: duration = 30s) -> Result<Upload>
upload.finish() -> Result<Stream>
```

`get`, `post` and `request` read the whole response. `post_json`, `put_json`,
`patch_json` and `request_json` send `json.encode(value)` as the body, with
`Content-Type: application/json` unless `headers` set a content type. Response
methods: `text() -> Result<str>`, `json() -> Result<json.Json>`, `ok() -> bool`
(200..=299), `header(name: str) -> str?` (case-insensitive; the first value),
`header_values(name: str) -> [str]`. HTTP/1.1 over TCP; `https` uses TLS with
the `tls` feature (as in `sand`). Up to 10 redirects (301, 302, 303, 307, 308)
are followed. Bodies may be delimited by `Content-Length`, chunked encoding or
connection close; one larger than `max_http_body` fails with kind `"limit"`.
Non-2xx statuses are **not** errors. At every hop, `timeout` bounds connecting
and receiving the head (then each wait for body data), and the capabilities
`Net(host:port)`, then `NetAddr` for the resolved addresses (§14.6), are
checked.

`open` returns once the response's head has arrived. Its `body`, an
`http.Body` (an `io.Reader`), is read as it arrives, so a body larger than
memory can be processed a chunk at a time (`io.copy(resp.body, file)`);
`max_http_body` applies only to its `read_all` and `read_text`. `Stream`
has `ok`, `header` and `header_values`, and implements `Close` (closing its
body). A body holds its connection, counted against `max_open_resources`,
until it is read to its end, closed (closing the connection), or collected.

Once a response's body is read to its end, its connection is kept for the
next request to the same origin, unless either side sent `Connection:
close`. Up to 16 idle connections are kept, 4 per origin, for up to 20
seconds; they count against `max_open_resources` and `max_memory`. A server
may close an idle connection just as a request is sent on it. If that happens before any
of the response arrives, a `GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS` or
`TRACE` request is sent again on a new connection; other requests fail.

`upload` connects and returns an `Upload`, an `io.Writer` for the request's
body: chunked, or exactly as long as a `content-length` in `headers` (a
write past it, or a `finish` short of it, fails with kind `"invalid"`).
`finish` ends the body and waits for the response's head; the response's
body is read from the upload's connection, which closing the `Upload`
closes. `timeout` bounds connecting and waiting for the head. Redirects are
not followed.

Server:

```sand
pub fn serve(ip: str, port: int) -> Result<Server>      // capability NetListen(ip:port)
server.accept() -> Result<Request>
server.addr() -> net.SocketAddr
pub struct Request { method: str, path: str, query: str, headers: [(str, str)], body: Body, conn: net.Conn }
request.respond(status: int, body: bytes, headers: [(str, str)] = []) -> Result<()>
request.respond_stream(status: int, headers: [(str, str)] = []) -> Result<Reply>
reply.finish() -> Result<()>
request.bytes() -> Result<bytes>
request.text() -> Result<str>
request.json() -> Result<json.Json>
request.header(name: str) -> str?                 // case-insensitive; the first value
request.header_values(name: str) -> [str]
```

`Server` and `Request` implement `Close` (`Server.close` stops accepting
and closes the connections being read; `Request.close` closes the
request's body and connection). `accept` returns a request once its head
has arrived; `path` and `query` are the parts of the target before and after `?`. The
handler reads the body (`request.body`, or `bytes`, `text`, `json`, which
read the rest of it, at most `max_http_body` bytes), each wait bounded by
30 seconds. A client that sent `Expect: 100-continue` is answered `100
Continue` when the handler starts reading the body.

`conn` is the client connection (fields of public types are public, §3.3).
The body is read from it; `conn` may be used between reads of the body
(reading `conn` reads past what the body has read), and closing the
connection or the request ends the body. `respond` writes a response with
a `content-length` header (and no body for `HEAD` requests or statuses
without one) to `conn`, then closes the request. `respond_stream` returns
a `Reply`, an `io.Writer` for a body framed as an upload's is (to an
HTTP/1.0 client without a `content-length`, the body ends with the
connection); the head is sent with the first write or `finish`, and
closing the reply finishes it. A script may also write to `conn` directly,
producing the HTTP framing itself, and close it. Constructing a `Request`
value directly has no special meaning.

After `respond` or `finish`, the server reads the client's next request
from the connection (and `conn` is closed) if the client allows it
(HTTP/1.1 without `Connection: close`, or HTTP/1.0 with `Connection:
keep-alive`) and the handler read the request's body to its end before
responding. Otherwise the response says `Connection: close` and the
connection is closed.

While `accept` waits, it reads the heads of requests concurrently from
the connections it accepts and those waiting for their next request,
returning the first that is complete; reading continues with the next
`accept`. A `Server` (opaque) counts against `max_open_resources` like a
listener, as does each connection being read, which on acceptance takes a
slot and an 8 KiB read buffer (counted against `max_memory`); while no
slot is free, connections wait in the listener's backlog. At most half of
`max_open_resources` connections are read at once: beyond that, the
earliest is answered `408 Request Timeout` and closed. A connection is
also answered and closed with `408` if its head is not complete within 30
seconds of acceptance (or of its first byte), `400 Bad Request` if the
head is malformed, and `503 Service Unavailable` if its buffer does not
fit in memory. A connection waiting for its next request is closed
without an answer if none of it arrives within 30 seconds, if newer
connections take its place, or if the run reaches `max_open_resources` or
`max_memory`.

On the web (§16.2), `http` is a client over the browser's `fetch` with the
same `Response`, `Stream`, `get`, `post`, `request` and `open`, and no
`upload` or `serve`. `Net(host, port)` is decided for the host as written before
anything is sent; the browser resolves names, so `NetAddr` is not checked.
A response is readable only if its site allows it (CORS), with the headers
it exposes. The browser sets some request headers itself and sends no
cookies. A redirect fails with kind `"http"`, since the browser hides its
target from the policy. `timeout` covers the whole exchange, and every body
is read whole, so `max_http_body` also applies to `open`.

### 13.5 `io`

Files (§13.3), connections (§13.10), HTTP bodies (§13.4) and the standard
streams implement two traits:

```sand
pub trait Reader {
    fn read(self, max: int = 65536) -> Result<bytes>  // at most max bytes, waiting until some are available; empty at the end
    fn read_line(self) -> Result<str?>                // without its \n or \r\n; None at the end; kind "invalid" if not UTF-8
    fn read_all(self) -> Result<bytes>                // the rest
    fn read_text(self) -> Result<str>                 // the rest as UTF-8 (kind "invalid" otherwise)
    fn lines(self) -> Iter<Result<str>>               // the remaining lines, read lazily; an error ends them
}
pub trait Writer {
    fn write(self, data: bytes) -> Result<()>         // all of data
    fn write_str(self, text: str) -> Result<()>       // all of text, as UTF-8
    fn flush(self) -> Result<()>                      // what the writer buffers, if anything
}
pub fn copy<R: Reader, W: Writer>(src: R, dst: W) -> Result<int>   // the bytes copied, a chunk at a time
pub let stdin: Stdin                                  // a Reader
pub let stdout: Stdout                                // a Writer
pub let stderr: Stderr                                // a Writer
```

`read` may return fewer bytes than are left. `read_text`, `lines`,
`write_str` and `flush` have default implementations; user types may
implement the traits.

Reading `stdin` needs the capability `Stdin`; writing `stdout` and `stderr`
needs `Stdout` and `Stderr`, allowed unless the policy denies them. A
denied write fails with kind `"permission_denied"` (`print` and `eprint`,
§12.1, fail). The output limit applies (§15.1).

The CLI buffers standard output like C's stdio (by line on a terminal, in
blocks otherwise). It flushes it on `io.stdout.flush()`, before writing to
standard error, and when every task is waiting. At the end of the run,
before a permission prompt and, on a terminal, before reading standard
input, it also waits until the output is written. Standard error is not
buffered. A separate thread writes both streams. When the destination does
not accept data (a full pipe), a script continues until 1 MiB of output is
pending, then waits; the run's limits and interrupts apply while it waits.

### 13.6 `env`

`get(name: str) -> str?` (capability `Env(name)`; a denial yields `None`),
`args() -> [str]` (arguments the host passed to the script; no capability),
`vars() -> [str: str]` (capability `Env("*")`; a denial yields an empty map).

### 13.7 `json`

```sand
pub enum Json { Null, Bool(bool), Int(int), Float(float), Str(str), Arr([Json]), Obj([str: Json]) }
pub native fn parse(text: str) -> Result<Json>
pub native fn encode<T>(value: T, indent: int = 0) -> str
pub native fn decode<reflect T>(text: str) -> Result<T>
pub native fn to_json<T>(value: T) -> Json
```

`parse` reads a number written without a fraction or exponent that fits in an
`int` as an `Int`, exactly; any other number (`1.0`, `1e3`, `-0`, or an
integer beyond the `int` range) as a `Float`. `Int(1)` and `Float(1.0)` are
not equal.

`Json` methods: `get(key: str) -> Json?`, `at(i: int) -> Json?`,
`as_str() -> str?`, `as_float() -> float?` (either number), `as_int() -> int?`
(an `Int`, or an integral `Float` within ±2^53), `as_bool() -> bool?`, `as_list() -> [Json]?`,
`as_map() -> [str: Json]?`, `is_null() -> bool`, `path(p: str) -> Json?`
(dot-separated keys and indices: `"items.0.name"`).

`encode` emits compact JSON for `indent == 0`, otherwise pretty-prints with
`indent` spaces. Object keys keep insertion order. Integers encode exactly.
Floats encode with a fraction or an exponent (`2.0`, `1e20`), so that they
parse back as floats; non-finite floats encode as `null`.

`encode`/`to_json` map values structurally: `int` → number (`Int`), `float`
→ number (`Float`), `str` →
string, `bool`, `()` → `null`, lists/sets/tuples → arrays, maps → objects
(non-`str` keys converted with `to_str`), structs → objects keyed by field
name, `None` → `null`, `Some(x)` → `x`; other enums: a payload-less variant
→ `"Variant"`, otherwise `{"Variant": [payload...]}` (an object for named
payloads); `bytes` → base64 string; `duration` → nanoseconds; value types
(§5.4) → their text form (`"2024-02-29"`); `Json` → itself.

`json!` (§6.13) builds a `Json` from JSON syntax with embedded Sand values.

`decode<T>` is the inverse for types built from those components. A struct
field missing from the object takes its declared default if that is a
constant expression (a literal, `[]`, `[:]` or `None`), else `None` if
optional; otherwise decoding fails with kind `"parse"` and a message naming
the path (e.g. `$.items[2].name: expected str`). Parsing is limited to a
nesting depth of 512.

### 13.8 `log`

`debug`, `info`, `warn`, and `error` emit structured log records with signature
`(msg: str, fields: [str: str] = [:])`. `set_level(level: str)` sets the minimum
script log level to `"debug"`, `"info"` (the default unless overridden by the
host, §16), `"warn"`, `"error"`, or `"off"`.

The engine drops records below the active script or host sink level, and counts
emitted records against `max_output_bytes`. If the host configures a log sink
(§16), the engine passes each enabled record to the sink. Otherwise, if the
policy allows `Stderr`, the engine writes `LEVEL message key=value ...` to
standard error, quoting empty messages, messages with control characters, and
empty keys or values or those containing whitespace, control characters, `"`, or
`=`.

### 13.9 `math`

Constants (`pub let` values): `pi`, `e`, `tau`. Functions on
`float`: `cbrt`, `exp`, `ln`, `log2`, `log10`, `log(x, base)`, `sin`,
`cos`, `tan`, `asin`, `acos`, `atan`, `atan2(y, x)`, `sinh`, `cosh`, `tanh`,
`hypot(x, y)`, `to_radians`, `to_degrees`,
`is_close(a, b, rel_tol: float = 1e-9, abs_tol: float = 0.0) -> bool`.
Integer helpers: `gcd(a: int, b: int) -> int`, `lcm`, `isqrt(n: int) -> int`.
Rounding, `abs`, `min`, `max`, `clamp`, `sqrt` and `pow` are methods of
`float` and `int` (§12.3, §12.4).

### 13.10 `net`

```sand
pub native fn listen(ip: str, port: int) -> Result<Listener>      // NetListen(ip:port)
pub native fn connect(host: str, port: int, timeout: duration = 30s) -> Result<Conn>      // Net(host, port)
pub native fn connect_tls(host: str, port: int, timeout: duration = 30s) -> Result<Conn>  // Net(host, port); feature tls
pub native fn split_host_port(text: str) -> Result<(str, int)>
listener.accept() -> Result<Conn>
listener.addr() -> SocketAddr
conn.peer_addr() -> SocketAddr
addr.ip() -> str
addr.port() -> int
```

`host` must be a hostname or unbracketed IP address (`"::1"`), `ip` must be an
IP address, and `port` must be in `0..=65535`; otherwise the call fails with
kind `"invalid"`. If `port` is `0`, `listen` and `http.serve` bind an ephemeral
port. An IPv6 listener accepts only IPv6 connections, and an IPv4-mapped IPv6
address (`::ffff:127.0.0.1`) is bound as IPv4. `split_host_port` parses
`"host:port"` or `"[ipv6]:port"`, or fails with kind `"parse"`. `SocketAddr` is
a value type (§5.4) formatted as `127.0.0.1:8080` or `[::1]:8080`.

`Listener` implements `Close`; `Conn` implements `io.Reader`, `io.Writer`
(§13.5) and `Close`. A connection's `read` returns at most what its 8 KiB
buffer holds; once the peer has finished sending, `read` keeps returning
empty bytes, `read_line` `None`, and `read_all` what was left. Closing a
listener or connection makes its pending and later operations fail with
kind `"closed"`. Different tasks may read and write the same connection.
Connections are made only to resolved addresses the policy allows (§14.6).

Listeners, connections and the sockets of operations in progress
(connections being established, pending `accept` calls, HTTP requests in
flight) count against `max_open_resources` until closed or collected; at
the limit (after a collection, §10.8), such operations fail with kind
`"limit"`.

`connect_tls`'s `timeout` bounds connecting and the TLS handshake together
(kind `"timeout"`). The server certificate is verified for `host` against
the bundled web PKI roots plus any the host added
(`Config::tls_root_certificates`, §16). Verification and handshake failures
are errors (kind `"invalid"` for certificate problems) that scripts can handle; the
same holds for `https` requests.

### 13.11 `random`

Engine-local PRNG (xoshiro256\*\*), seeded from host entropy unless the host
sets a seed. `int(lo: int, hi: int) -> int` (uniform in `lo..hi`; fails if
empty), `float() -> float` (`[0, 1)`), `bool() -> bool`,
`choice<T>(xs: [T]) -> T?`, `shuffle<T>(xs: [T])`, `sample<T>(xs: [T], k: int)
-> [T]`, `bytes(n: int) -> bytes`, `seed(s: int)`. `random.rng(seed: int) -> Rng` returns a
generator independent of the engine's, with the same methods except `seed`.

### 13.12 `regex`

```sand
pub native fn new(pattern: str) -> Result<Regex>
regex.is_match(pattern, text) -> Result<bool>      // convenience
r.is_match(text) -> bool
r.find(text) -> Match?
r.find_all(text) -> [Match]
r.captures(text) -> Captures?
r.captures_all(text) -> [Captures]
r.replace(text, rep: str) -> str        // first; $1, ${name} expand
r.replace_all(text, rep: str) -> str
r.split(text) -> [str]
r.pattern() -> str
struct Match { text: str, start: int, end: int }
struct Captures { text: str, spans: [int], regex: Regex }
Captures: get(i: int) -> Match?, name(n: str) -> Match?, len() -> int, text(i: int) -> str?
```

`Captures` holds the searched text and each group's start and end offsets
(`-1` for a group that did not participate); its methods slice the text.
Syntax is that of the Rust `regex` crate (linear-time, no backreferences);
patterns with backslashes are best written as raw strings (`r"\d{4}"`),
since `\d` is an invalid escape in an ordinary string; braces need no care
(`"[0-9]{4}"`, §2.5). Each compiled
automaton is limited to a few MiB (larger patterns, such as `\w{150}`, fail
with kind `"limit"`). A compiled regex counts against `max_memory` with its
automata and its search cache, which grows with use up to a few MiB.

### 13.13 `statistics`

On `[float]`: `mean`, `median`, `variance` (sample), `stdev` (sample),
`pvariance`, `pstdev`, `min`, `max`, `sum`, `mode -> float?` (most common, first
on ties), `quantile(xs, q: float) -> float?` (linear interpolation, `0 <= q <=
1`), `correlation(xs, ys) -> float?`, `linear_regression(xs, ys) -> (float,
float)?` (slope, intercept). All except `sum` return `float?` and are `None` for
empty input (variance/stdev need at least two values). Also `mean_int([int])
-> float?`.

### 13.14 `test`

`assert_eq<T: Eq>(actual: T, expected: T, msg: str = "")`, `assert_ne`,
`assert_true(c: bool, msg: str = "")`, `assert_false`,
`assert_close(a: float, b: float, tol: float = 1e-9)`,
`assert_err<T, E>(r: Result<T, E>)`, `assert_ok`, `fail(msg: str) -> never`,
`property<T>(name: str, gen: fn(random.Rng) -> T, prop: fn(T) -> bool,
cases: int = 100, seed: int = 20240611)` (runs `prop` on `cases` values
`gen` produces from a `random.Rng` seeded with `seed`; on failure fails
with the failing input's debug form). The fixed default seed makes property
tests reproducible; pass another `seed` to explore other inputs. `assert_eq`
fails with `assertion failed: left == right\n  left: <debug>\n right: <debug>`.

### 13.15 `time`

Dates, times, and time zones modeled on the JavaScript Temporal API. Uses the
ISO 8601 calendar (years -9999 to 9999), nanosecond precision, and a bundled
IANA time zone database.

| Type             | Meaning                                  | Text form                                     |
| ---------------- | ---------------------------------------- | --------------------------------------------- |
| `Instant`        | exact point in time                      | `2024-03-10T07:30:00Z`                        |
| `ZonedDateTime`  | instant in a time zone                   | `2024-03-10T03:30:00-04:00[America/New_York]` |
| `PlainDateTime`  | calendar date and wall-clock time        | `2024-03-10T03:30:00`                         |
| `PlainDate`      | calendar date                            | `2024-03-10`                                  |
| `PlainTime`      | wall-clock time                          | `03:30:00`                                    |
| `PlainYearMonth` | year and month                           | `2024-03`                                     |
| `PlainMonthDay`  | month and day                            | `03-10`                                       |
| `Span`           | signed calendar and clock units          | `P1M2DT3H`                                    |
| `TimeZone`       | IANA zone, `UTC`, or fixed minute offset | `America/New_York`                            |

All nine types are value types (§5.4). `Span` and `TimeZone` implement `Hash`;
the other types implement `Ord + Hash` in chronological order (`ZonedDateTime`
is ordered by instant, then by zone ID). In a `Span`, sub-second units are
normalized into seconds (`1500ms` becomes `1s` and `500ms`). Two `Span` values
are equal if all ten unit fields are equal (`P1D != PT24H`). Two `TimeZone`
values are equal if their canonical IDs are equal (`Etc/UTC != UTC`). Each type
is formatted, JSON-encoded, and JSON-decoded in its ISO 8601 / RFC 9557 text
form. `T.parse(text)` parses this text form (or a `strftime` pattern if
`format` is supplied) and returns an error of kind `"parse"` on invalid input.

Constructors:

- `date(year, month, day)`, `time(hour, minute = 0, second = 0, millisecond = 0, microsecond = 0, nanosecond = 0)`, `datetime(year, month, day, hour = 0, ...)`, `year_month(year, month)`, and `month_day(month, day)` validate their components and return `Result<T>` (error kind `"invalid"`).
- `span(years = 0, months = 0, weeks = 0, days = 0, hours = 0, ..., nanoseconds = 0)` fails with kind `"invalid"` if units have mixed signs.
- `zone(name) -> Result<TimeZone>` accepts an IANA name (case-insensitive), `UTC`, or a whole-minute offset within `±23:59` (`+00:00` returns `UTC`), and returns an error of kind `"not_found"` otherwise.
- `Instant.from_epoch_seconds`, `from_epoch_millis`, `from_epoch_nanos`; constants `UTC` and `MIDNIGHT`.

Clocks:

- `now() -> Instant` returns the current wall-clock time.
- `local_zone() -> TimeZone` returns the configured host time zone (§16) if the policy permits `TimeZone`, or `UTC` if denied.
- `now_zoned(zone = local_zone()) -> ZonedDateTime` and `today(zone = local_zone()) -> PlainDate`.
- `stopwatch() -> Stopwatch` starts a monotonic timer; `sw.elapsed() -> duration` returns the elapsed time.

Methods:

- Accessors: `year`, `month`, `day`, `day_of_week` (1 = Monday .. 7 = Sunday), `day_of_year`, `week_of_year`, `year_of_week` (ISO 8601 week calendar), `days_in_month`, `days_in_year`, `in_leap_year`, `hour`, `minute`, `second`, `millisecond`, `microsecond`, `nanosecond` on applicable types; `offset() -> duration`, `time_zone() -> TimeZone`, and `hours_in_day() -> float` on `ZonedDateTime`; `epoch_seconds`, `epoch_millis` (floored), and `epoch_nanos` on `Instant` and `ZonedDateTime`; `id() -> str` on `TimeZone`.
- `with(...)` replaces specified components and clamps values that exceed their upper bound (e.g. April 31 becomes April 30). Fails with kind `"invalid"` if `month < 1`, `day < 1`, or `year` is outside `-9999..=9999`. On `ZonedDateTime`, `with` preserves the offset if it remains valid for the new wall-clock time; otherwise gaps and folds are resolved using `disambiguation: Disambiguation`.
- `add(amount)` and `subtract(amount)`: `Instant` accepts `duration`; `ZonedDateTime` accepts `Span` only (so `1d`, which is 24 hours, is not confused with a calendar day); plain types accept `Span` or `duration` via `ToSpan`. Calendar units advance the date and clamp the day to the end of the month, then clock units advance exact time. `PlainDate` truncates sub-day units to whole days, `PlainTime` wraps around midnight and ignores day and larger units, and `PlainYearMonth` rejects units smaller than a month.
- `until(other, ...)` and `since(other, ...)` compute the difference from `largest: Unit` down to `smallest: Unit`, rounded to a multiple of `increment` using `mode: RoundingMode` (default `Trunc`). `Instant` and `PlainTime` return `duration`; other types return `Span`.
- `round(smallest, increment = 1, mode = RoundingMode.HalfExpand)` rounds to a multiple of `increment` in unit `smallest`.
- Conversions: `to_instant`, `to_zoned_datetime(zone)` (takes `disambiguation: Disambiguation = Compatible` on `PlainDateTime`; uses midnight on `PlainDate`), `to_plain_datetime`, `to_plain_date`, `to_plain_time`, `to_plain_year_month`, `to_plain_month_day`, `PlainYearMonth.to_plain_date(day)`, `PlainMonthDay.to_plain_date(year)`, `with_time_zone(zone)`, `with_plain_time(time)`, `start_of_day()`, `next_transition()`, and `previous_transition()`.
- `format(pattern: str) -> str` formats a value with `strftime` directives (`Instant` is formatted in UTC).
- `Span` methods: unit accessors (`years()` .. `nanoseconds()`), `sign()`, `is_zero()`, `negated()`, `abs()`, `with(...)`, `add`, `subtract`, `round(smallest, largest, increment, mode)`, `total(unit) -> float`, `compare(other) -> int`, and `to_duration()`. The last six methods accept `relative_to: ZonedDateTime?`; if `relative_to` is `None`, days are 24 hours, weeks are 7 days, and spans with nonzero years or months fail with kind `"invalid"`.

Results outside years `-9999..=9999`, `epoch_nanos` outside years `1677..=2262`, and durations outside the 64-bit nanosecond range fail with kind `"overflow"`. Invalid options fail with kind `"invalid"`. Use `task.sleep(d)` to sleep.

### 13.16 `task`

See §11.3.

### 13.17 `uri`

```sand
pub struct Uri {
    scheme: str, userinfo: str?, host: str?, port: int?,
    path: str, query: str?, fragment: str?
}
pub fn parse(text: str) -> Result<Uri>                 // RFC 3986
pub native fn encode_component(s: str) -> str          // percent-encodes all but unreserved
pub native fn encode_path(s: str) -> str               // also keeps "/"
pub native fn decode_component(s: str) -> Result<str>
pub native fn parse_query(q: str) -> [(str, str)]      // '+' decodes to space
pub native fn build_query(pairs: [(str, str)]) -> str
```

`Uri` methods: `to_str()` (recomposition), `join(reference: str) ->
Result<Uri>` (RFC 3986 §5.2 resolution), `query_pairs() -> [(str, str)]`,
`authority() -> str?`, `default_port() -> int?` (from the scheme: http 80,
https 443, ws 80, wss 443, ftp 21).

### 13.18 `uuid`

`v4() -> str`, `v7() -> str` (time-ordered), `nil() -> str`,
`parse(s: str) -> Result<str>` (accepts hyphenated, simple, braced and `urn:uuid:`
forms; returns lower-case hyphenated), `is_valid(s: str) -> bool`,
`version(s: str) -> int?`.

---

## 14. Sandbox and policy

### 14.1 Principles

- Scripts have no access to the outside world except through standard library
  and host native functions.
- Every such access is described by a **capability request** and checked by the
  engine's policy **before** the effect occurs.
- The VM has no FFI, does not spawn processes, does not use IPC, shared memory
  or `mmap`, and never executes generated machine code.
- Paths are normalized lexically (`.`/`..` resolved). The longest existing
  prefix is then resolved like `realpath` (symlinks followed; `..` in a link
  target is the parent of the link directory). The engine checks the result so
  `..` and symlinks cannot escape an allowed directory. A symlink whose target
  cannot be resolved is not followed (§13.3). Memory file system paths (§16.1)
  are only normalized lexically.

On the host, a path is resolved by walking recursively from root to subpaths
without following symlinks, keeping subdirectory file descriptors open, and accessing
the next subpath with `openat` to avoid any potential for symlink swapping.
On macOS, the engine resolves existing entries to on-disk names and strips firmlinks
(`/System/Volumes/Data/Users/...` is `/Users/...`). Other Unix systems
preserve path names as supplied. Windows paths are resolved the same way,
treating junctions as symlinks. On Windows, the engine resolves entries to
on-disk names, compares paths case-insensitively, and normalizes paths to standard
drive or UNC format (not `\\?\...`). On Windows, `rename` works by
path, but keeps every directory on both paths open so none
can be renamed or replaced. A single race remains: an entry's parent can be
emptied and converted into a junction during rename.

### 14.2 Capabilities

| Capability                  | Raised by                                  |
| --------------------------- | ------------------------------------------ |
| `FsRead(path)`              | reading files, listing dirs, metadata      |
| `FsWrite(path)`             | creating, writing, removing, renaming      |
| `Net(host, port)`           | outgoing TCP and TLS connections (`net`)   |
| `Http(method, url)`         | each `http` request and redirect hop       |
| `NetAddr(host, addr)`       | each address `host` resolved to (§14.6)    |
| `NetListen(ip:port)`        | listening sockets                          |
| `Env(name)`                 | environment variable reads (`"*"` for all) |
| `Stdin`, `Stdout`, `Stderr` | standard streams                           |
| `TimeZone`                  | `time.local_zone()`                        |
| `Import(path)`              | loading a script module                    |
| `Custom(name, detail)`      | host native modules may define their own   |

### 14.3 Decisions

A policy maps a request to `Allow`, `Deny` or `Prompt`. On `Prompt` the
engine suspends execution and calls the host's prompter, waiting for an
answer of `AllowOnce`, `AllowAlways`, `DenyOnce` or `DenyAlways`. If no
prompter exists, any would-be prompt is automatically denied. An "Always"
decision persists for the engine lifetime across all runs and compilations.
A "Once" decision applies to a single request.

`Import` requests are determined during compilation by `check` and `lint`,
once per script module, depth-first in import order, with the module's
resolved absolute path.

### 14.4 Effects of a denial

- I/O functions return `Err` with kind `"permission_denied"`.
- A denied environment variable is treated as unset.
- `print` and `eprint` fail (kind `permission_denied`) with `permission denied: stdout` / `stderr`
- writes to `io.stdout` and `io.stderr` fail with kind `"permission_denied"`.
- A denied `Import` raises a compile-time error.

### 14.5 Built-in policies

- `Policy::deny_all()` denies everything except `Stdout` and `Stderr`.
- `Policy::allow_all()` allows everything. Only use this when running 100% trusted code.
- `RulePolicy` uses a rule policy + prompter for its allow and deny rules.
- `deny_private_networks()` denies private and internal network destinations (§14.6).

A policy decision is determined by the most specific matching rule.
If two opposing rules have equal specificity, deny wins. Rules:

- **Files** (`FsRead`, `FsWrite`, `Import`)
  - a path pattern matches the path and its subpaths: `/data` covers `/data/a/b`
  - Each path segment may be a string, a name with `*` wildcards (`*.pem`),
    standalone `*`, or standalone `**`.
  - A path is "more specific" if it has more matching segments. For example,
    `deny ~/**/*.pem` is more specific than `allow ~/project` for `~/project/key.pem`
  - If two paths have equal depth, the one with fewer wildcards is more specific
  - Write access implies read access
  - Read denial implies write and import denial

- **Network Requests** (`Net`)
  - a host pattern may match any domain `*`, a specific domain `example.com` or strict subdomains `*.example.com`
  - a host pattern may also match an IP address (`127.0.0.1`, `::1`) or a CIDR range (`10.0.0.0/8`, `fc00::/7`),
    optionally with a port (`host:443`, `[::1]:8080`, `10.0.0.0/8:443`, `[fc00::/7]:443`).
  - Exact hosts and addresses are most specific, then ranges and domains (longer ones more), then `*`
  - A port increases rule specificity.
  - Host names must be ASCII and are compared case-insensitively, ignoring a trailing dot
  - IPv4-mapped IPv6 addresses are compared as IPv4.
  - Address deny patterns are syntax-agnostic: `0.0.0.0` and `::` are considered identical.

- **HTTP(S) requests** (`Http`)
  - inherits net rules for the host pattern & port
  - adds http methods and paths
  - Specificity compares the host (as for `Net`), then http scheme, then path, then methods.
  - URL paths are compared percent-decoded, without `.` and `..` segments
  - A request whose path contains `\` or an encoded separator (`%2F`, `%5C`) is allowed
    only if it is also allowed with those taken as separators.
  - A net rule allowing a host lets scripts send it anything over `net`, even if `deny http` rules say otherwise

- **Environment variables** (`Env`)
  - supports a name `AWS_SECRET_KEY` or a pattern (`AWS_*`)
  - specific names are prioritized above wildcard patterns

- **Listen addresses** (`NetListen`)
  - patterns are `ip:port`, `[ipv6]:port`, `ip`, `localhost` (any loopback address), `*:port`, or `*`
  - host `*` matches any address, port `*` matches any port, and port `0` matches only an ephemeral port request
  - IPv4-mapped IPv6 addresses are compared as IPv4
  - specificity compares the host (`ip` or `localhost` before `*`), then the port

- **Custom capabilities**:
  - a native module may define its own capabilities

Path rules are compared after normalization (§14.1). A rule's path is
resolved when the policy decides its first request. Allow rules compare
names exactly, ignoring case on Windows. Deny rules, on every platform,
compare case-insensitively and normalization-insensitively (canonical
decomposition, full case folding, default-ignorable characters removed), so
they also cover other spellings, including of paths that do not exist yet
(`data/ſecrets`, `café` precomposed or with a combining accent)

On case-insensitive file systems, the engine resolves existing paths to
on-disk names (`data/SECRETS` becomes `data/secrets`). For directory entry operations (§13.3),
`Policy::check_entry` evaluates the on-disk name without resolving symlinks in the last path component.
If the system cannot read an on-disk name (a symlink outside macOS, a device, or an unopenable file),
the engine scans the parent directory if it contains at most 4096 entries.

A directory that holds an entry decided by a deny rule cannot be renamed, replaced
or recursively removed as a whole. The tree is scanned for such entries only if
`Policy::denies_under` reports that a deny rule may match a path in it. For
custom policies (`CustomPolicy`), the engine invokes `denies_under` and `protects`
on the tree. By default, the engine scans trees and protects entries when reading
or writing the entry is denied.

### 14.6 Network destinations

An outgoing connection (`net.connect`, `net.connect_tls`, `http` requests and redirect hops)
is checked in two stages:

1. `Net(host, port)` (for `http`, `Http(method, url)`) for the host name or
   IP literal the script used. This request may be prompted.
2. The name is resolved once (an IP literal to itself), and each address is
   checked as `NetAddr(host, addr)`. Connection is attempted only to allowed
   addresses, in resolver order. If all are denied, the operation fails with
   kind `"permission_denied"` and the address. `NetAddr` never prompts: `Prompt`
   counts as `Allow`, since the user already decided the first request.

An internal address is any non-public network. Examples include `0.0.0.0/8`,
loopback, RFC 1918, shared/CGNAT `100.64.0.0/10`, link-local
(`169.254.0.0/16`, `fe80::/10`), unique-local `fc00::/7`, site-local,
multicast, broadcast and other reserved, documentation and benchmarking
ranges, with IPv6 forms embedding an IPv4 address (IPv4-mapped,
IPv4-compatible, NAT64 `64:ff9b::/96`, 6to4). Rules:

- An internal IP literal, or `localhost`, is allowed at stage 1 only by an
  explicit rule or a user prompt response; `localhost` must then resolve to
  loopback addresses.
- An internal address reached by resolving any other name is denied unless
  an address net rule allows it.

### 14.7 Policy files

A policy file (`.policy`) contains policy rules and limits as text. Read it with
`sand run --policy=FILE` (§17.5); parse it with `sandlang::policy::PolicyFile::parse(text, base, home)`.
Each line contains an independent statement. A comment begins with `#` and
continues for the rest of the line. Words are separated by spaces or commas.
Quoted text `"..."` (with `\"` and `\\` escapes) is parsed literally, so a quoted
`*` is not considered a wildcard. A word may contain both quoted and unquoted text
(`"/my data"/**`).

```policy
# Read the system and the project; keep secrets out.
let owners = alice bob
allow read /usr /System ~/.gitconfig
allow ./                                 # read and write the project
deny ./**/.env                           # the more specific rule wins
deny ~/Documents/*
allow http GET HEAD api.github.com
allow http POST PATCH api.github.com/repos/{owners}/**
allow env HOME USER LC_*
default prompt                           # ask about the rest (default: deny)
limit memory 64M
```

| Statement                                                 | Meaning                                                                         |
| --------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `allow PATH...`, `deny PATH...`                           | all access to files (§14.5): `allow` reads and writes, `deny` denies everything |
| `allow read PATH...`, `deny read PATH...`                 | `FsRead` (a deny also denies writing and importing)                             |
| `allow write PATH...`, `deny write PATH...`               | `FsWrite` (an allow also allows reading)                                        |
| `allow import PATH...`, `deny import PATH...`             | script imports (an allow restricts imports to its paths)                        |
| `allow net HOST...`, `deny net HOST...`                   | connections, and any HTTP request, to hosts (§14.5)                             |
| `allow http [METHOD...] URL...`, `deny http ...`          | HTTP requests                                                                   |
| `allow private-net`                                       | do not deny private and internal destinations (§14.6)                           |
| `allow listen ADDR...`, `deny listen ADDR...`             | listening addresses (§14.5)                                                     |
| `allow env NAME...`, `deny env NAME...`                   | environment variables (`AWS_*`; `*`: all)                                       |
| `allow stdin`, `deny stdin`, `deny stdout`, `deny stderr` | standard streams                                                                |
| `allow timezone`, `deny timezone`                         | the local time zone                                                             |
| `allow custom NAME [DETAIL]`, `deny custom NAME [DETAIL]` | a host-defined capability (any detail if omitted)                               |
| `default deny` \| `default prompt` \| `default allow`     | fallback decision for unmatched requests (once per file)                        |
| `limit NAME VALUE`                                        | a limit (§15.1), below                                                          |
| `let NAME = VALUE...`                                     | a set: `{NAME}` in any rule's word stands for each value                        |

All paths must be anchored, but they may be prefixed with a shortcut:

- `/...` begins at the root directory
- `./...` begins at the file's directory
- `../...` begins at the file's parent directory
- `~/...` begins at the user's home directory

URL patterns are `[scheme://]host[:port][/path]`: a host pattern (§14.5,
without ranges), then path segments as for files, percent-decoded. A pattern
without a port but with a scheme matches the scheme's default port. A pattern
without a port or a scheme matches any port.

HTTP Methods (`GET`, any case, or any upper-case word) precede URLs.
If no method is present, an http rule matches every method.

A word with multiple set references allows every combination of set values
(`{owners}/{repos}`), limited to 100,000 combination. A set may be defined
anywhere in the file. A set may not contain another set.

If the policy has no `default` rule, the host determines unmatched requests.
By default, they are denied. If a prompter is defined, the host prompts and
returns a response. Without `allow private-net`, private and internal destinations
are denied as by `deny_private_networks()`.

A policy file may contain limit rules:

- `memory`, `output`, `source` and `http-body` take sizes (`4096`, `64K`, `256M`, `1G`)
- `time` (wall clock) and `cpu` durations (`500ms`, `10s`, `1.5m`, `2h`, `1d`)
- `instructions`, `compile-fuel`, `tasks`, `call-depth` and `open-resources` counts (`1_000_000`)
- `none` removes `memory`, `time`, `cpu`, `instructions`, `compile-fuel` and `output` limits.

Each system resource category that is not explicitly specified has a default value.

A policy file with any error is rejected. Each `PolicyError` provides the line and
column containing the error.

Policy files are covered by `sand fmt`, `sand lint` and `sand check` (§17).
The formatter separates words by single spaces, adds quotes where necessary,
writes methods in upper case, aligns the trailing comments of consecutive
lines and removes runs of blank lines. `sandlang::policy::lint_policy`
reports:

| Lint                   | Description                                                                           |
| ---------------------- | ------------------------------------------------------------------------------------- |
| `redundant-rule`       | a rule repeats another, or a broader rule already decides what it covers the same way |
| `overridden-rule`      | a rule taking precedence decides everything a rule covers the other way               |
| `bypassable-http-deny` | a `deny http` rule for a host that a net rule lets scripts connect to                 |
| `unused-set`           | a `let` set that is never referenced in a rule                                        |

The first two compare paths as written, without resolving symlinks, and report
a rule only when a single other rule shows that it is not needed.

`sandlang::policy::rule_for(capability, allow)` gives, as a policy file
line, the rule that decides a promptable request for good (e.g. `allow read
/data/a.txt`, `deny http GET https://example.com/a`), quoted as needed; the
playground adds it to the policy when "always" or "never" is answered.

---

## 15. Resource limits

### 15.1 Limits

Configurable per engine:

| Limit                | Default    | Effect when exceeded                                                                                                                                           |
| -------------------- | ---------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `max_memory`         | 512 MiB    | run fails: `memory limit exceeded` (I/O whose buffers do not fit fails with kind `"limit"`)                                                                    |
| `max_instructions`   | unlimited  | run fails: `instruction limit exceeded` (counted in fuel units, §15.2)                                                                                         |
| `max_cpu_time`       | unlimited  | run fails: `cpu time limit exceeded` (the engine thread's CPU time executing, excluding waits and prompts; without a thread CPU clock, elapsed time executing) |
| `max_wall_time`      | unlimited  | run fails: `time limit exceeded` (covers compiling and running, from the start of compilation, excluding waits for prompt answers)                             |
| `max_call_depth`     | 10,000     | fails: `limit: stack overflow`                                                                                                                                 |
| `max_tasks`          | 10,000     | `spawn` fails: `limit: task limit exceeded`                                                                                                                    |
| `max_open_resources` | 1,024      | opening files, connect/listen/accept and HTTP requests fail with kind `"limit"` (after a collection, §10.8; pending operations' sockets count, §13.10)         |
| `max_output_bytes`   | unlimited  | writes fail: `output limit exceeded`                                                                                                                           |
| `max_http_body`      | 64 MiB     | HTTP read fails with kind `"limit"`                                                                                                                            |
| `max_source_bytes`   | 16 MiB     | compile error: `source too large` (the main file and all imported scripts together; the standard library is not counted)                                       |
| `max_compile_fuel`   | 10,000,000 | compile error: `program too complex to compile` (§15.3)                                                                                                        |

Memory is the engine's accounting, enforced after garbage collection, of
live heap objects (with their buffers and map and set indexes), tasks
(call stacks, and lists of tasks waiting for a task or channel; a call that
would grow a stack beyond the limit fails with the memory limit), and the
buffers of pending I/O (file and socket reads, HTTP requests and responses
in transfer, standard input, copies of data being written) from the start
of the operation until its result becomes a value. An I/O operation whose
buffers would not fit fails with kind `"limit"` before allocating them and
the run continues, so a peer sending a huge line or body cannot end a
server script; files are checked by size before reading, network data as it
arrives. Open resources also count their fixed buffers: a connection's 8
KiB (48 KiB for TLS, including an estimate of the session's buffers) from
when it is being connected or accepted until closed, and a file's 16 KiB
read buffer from its first buffered read. These must leave the last 16 KiB
of `max_memory` free, so a script can still handle the error: opening a
connection that would not fails with kind `"limit"`, and an HTTP server
answers it `503 Service Unavailable` and closes it. Library functions also
check the Rust-side scratch space of a result they build (output strings
and lists, display and JSON text, sort buffers) before or while it grows,
collecting garbage before reporting the limit, so a script cannot make the
host allocate much beyond `max_memory`.

In a session (§16.3, §17.6), the instruction, CPU time, wall time and
output limits apply to each input; the memory limit to the session's
persistent heap. Limit violations are reported to the host as distinct
error variants and cannot be intercepted by scripts. The host may also
interrupt an engine from another thread with an `InterruptHandle`, which
stops compilation, bytecode, long-running library functions, and waits for
I/O or prompt answers.

### 15.2 Fuel

Execution is measured in _fuel units_. The interpreter spends one unit per
function call, loop iteration and backward jump, plus one unit per 64
instructions the step may run before the next charge: a call (or task
start) per 64 instructions of the called function, an iteration per 64
instructions of the loop, and creating a closure per 64 captured values.
Work a library function or generic operation does in one step is charged in
proportion to its size, roughly one unit per simple loop iteration's worth
(a few to a few tens of nanoseconds):

| Work                                                                                        | Cost                                                                 |
| ------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| element visited, compared, hashed, copied or produced                                       | 1 unit                                                               |
| byte copied, compared or scanned (strings, bytes, output text)                              | 1/64 unit                                                            |
| byte processed one at a time (hashing, regex search, encodings, case conversion, JSON text) | 1/8 unit                                                             |
| compiling a regex                                                                           | 1,000 + 16 per pattern byte, + 1/8 unit per byte of compiled program |
| regex work (compiling, searching, iterating), at least                                      | 1 unit per microsecond it takes                                      |

Elements include list, map and set elements, the components of values being
compared, hashed, displayed or encoded, JSON values, regex matches and sort
comparisons. Constant-time operations are free. For example, sorting a list of
n numbers costs about n·log₂(n) units, `==` on two lists of n integers about n
units, and `json.parse` about one unit per value plus one per 8 bytes of
text.

Regex work is charged by time too because it is not always proportional to
the text: with leftmost-first semantics a search may look far past its
match (iterating over the matches of `[a-z]*X|[a-z]` in a long run of
letters scans the rest of the text per match), and searches the lazy DFA
cannot handle fall back to an engine whose cost per byte grows with the
pattern. Time is measured as the function runs and charged between matches
when iterating. Ordinary searches take a few to a hundred nanoseconds per
unit of their deterministic charge, so only work that charge underestimates
by an order of magnitude or more is charged by time.

Fuel counts against `max_instructions`. Once a scheduling slice's fuel is
spent, long operations check the instruction budget, the CPU and
wall-clock limits and the interrupt flag at least every 20,000 units. The
limit is exact for library functions (a call stops as soon as the run's
fuel exceeds it) and for bytecode (within one unit); the interpreter's
generic operations (comparison, display, concatenation, slicing) may
overrun it by the bytecode fuel spent since the last library call, less
than 20,000 units. Operations that cannot be interrupted internally (a
single regex search) are charged before they start, so they fail at once
when the remaining budget cannot cover them. A task whose library calls use
up its slice is preempted when the call returns. System calls that may block,
such as file system operations and writes to standard output, run on other
threads. While the run waits for them, its limits and interrupts still apply.

### 15.3 Compile-time limits

Checking a program happens before it runs, so it is bounded separately:

- **Compile fuel.** Type checking spends about one unit per expression
  checked, unification step, type component examined or produced, and pattern
  row examined; code generation one unit per expression compiled (deferred
  code once per copy, §7.1), instruction emitted and expression examined by
  its analyses. Ordinary programs need less than one unit per byte of source
  (a `match` with 1,000 arms on an enum about a million). When
  `max_compile_fuel` is exhausted, compilation fails with `program too complex
to compile`. `None` means unlimited.
- **Time and interrupts.** Compilation stops with `time limit exceeded` once
  `max_wall_time` has elapsed since it started (the run that follows has
  what is left), and with an interrupt error when the host interrupts the
  engine. The standard library is compiled once per process (or per engine
  with host modules), outside any program's budget.
- **Local limits**, reported at the offending construct: nesting of
  expressions, types, patterns and blocks to 256 levels (`nesting too
deep`); strings nested inside string interpolations to 32 levels
  (`strings nested too deeply inside string interpolations`); types to
  10,000 components (`type too large`); the exhaustiveness check of one
  `match` to a fixed amount of work (`pattern too complex to check for
exhaustiveness`); copies of deferred code (§7.1) in one function to about
  a million instructions (`too much deferred code`); runtime type
  descriptions of `reflect` type arguments (e.g. for `json.decode`), which
  describe each struct or enum instance once, shared wherever it occurs,
  and each distinct type once per program: one description may take
  100,000 units of work (about one per component built or type component
  examined), all of a compilation 1,000,000 (`type too large to describe at
runtime`), and a polymorphically recursive type such as `struct P<T> {
next: P<[T]>? }` cannot be described.
- **Diagnostics.** At most 50 syntax errors per module, and 50 type errors,
  are reported.
- **Memory.** Compilation is not counted against `max_memory`; it uses
  several hundred bytes per source byte, bounded by `max_source_bytes`,
  which hosts compiling untrusted code under a small memory limit should
  lower accordingly.

---

## 16. Embedding API (Rust)

```rust
use sandlang::{Engine, Config, Limits, Policy, NativeModule, Value};

let mut module = NativeModule::new("geo");
module.function("dist", |a: f64, b: f64| -> f64 { (a * a + b * b).sqrt() });
module.async_function("lookup", |key: String| async move { Ok::<_, String>(key.len() as i64) }).params(&["key"]);

let config = Config::new()
    .policy(Policy::deny_all())
    .limits(Limits { max_memory: Some(64 << 20), ..Limits::default() })
    .module(module);
let mut engine = Engine::new(config);
let outcome = engine.run_source("main.sand", r#"import geo; print(geo.dist(3.0, 4.0))"#).await?;
```

- `Engine: Send`. All script execution happens inside the future returned by
  `run_*`, on whichever thread polls it. Engines share no mutable state, so any
  number may run concurrently on different threads or tasks (or, on the web,
  interleaved on one thread, §16.2).
- `run_source(name, source)`, `run_file(path)`, `check_source`, `run_tests`,
  `lint_source`, `lint_file` (lint warnings, §17.2).
- Captured streams: `Config::stdout(Box<dyn OutputSink>)`, `stderr`,
  `stdin_bytes`. With `Config::stdin_feed(&StdinFeed)`, scripts read what
  the host `send`s (`end()` ends one read); `wanted()` is resolved if a script
  is waiting for input and has nothing else to run.
- `Config::log_sink(impl LogSink)` routes `log` records to a host sink as
  `LogRecord { level, message, fields, location }` on the executing thread
  instead of writing to standard error. `LogSink::enabled(level)` filters
  records before the engine constructs a `LogRecord`; any
  `Fn(&LogRecord) + Send + Sync` implements `LogSink` for all levels. `Display`
  formats `LogRecord` in the default standard error format.
  `Config::log_level(LogLevel)` sets the initial script log level (default
  `Info`).
- `Config::memory_fs(MemFs)` gives scripts a memory file system (§16.1).
- `Config::library(dir)` sets the directory of library scripts for
  `import <name>` (§3.2). `Engine::module_names()` returns the registered
  standard and host module names, which take precedence over library scripts.
- `Config::clock(Arc<dyn platform::Clock>)` sets the clock and timer provider
  for the engine. The engine uses this provider for system time, monotonic time,
  UUIDv7 generation, sleep timers, execution limits, and network timeouts.
  The engine reports consumed fuel through `Clock::work` and yields execution
  through `Clock::yield_now`. `MemFs::with_capacity_and_clock` sets file
  modification timestamps from this clock.
- `Config::network(Arc<dyn network::Network>)` sets the network provider for
  host resolution, outgoing TCP streams, and TCP listeners in `net` and `http`.
  The engine checks the network policy before connecting or listening, and
  negotiates TLS over the returned stream. `Network::listen` must bind only the
  requested address; an IPv6 listener must not accept IPv4 connections.
- `Config::time_zone(sandlang::time::TimeZone)` sets the time zone returned by
  `time.local_zone()` if the policy allows `TimeZone`. Use `TimeZone::get(name)`
  to look up a zone by name or offset, or `TimeZone::system()` (feature `os`)
  to read the operating system's IANA time zone.
- `Config::tls_root_certificates(Vec<Vec<u8>>)` trusts additional
  DER-encoded root certificates (e.g. a private CA) besides the bundled web
  PKI roots; calls accumulate. An invalid one makes TLS connections fail
  with kind `"io"`. Without the `tls` feature it has no effect.
- Policies (§14): `Policy::check(&Capability)`, `Policy::check_entry`,
  `Policy::denies_under(path)`;
  `RulePolicy::{allow_read, allow_http, deny_net, deny_private_networks, ...}`;
  `sandlang::policy::{PolicyFile, format_policy, lint_policy, rule_for}`
  for policy files; `sandlang::policy::{is_private_address,
normalize_path, normalize_entry_path}` for hosts writing their own
  policies or natives.
- `NativeModule::function` accepts closures whose argument and return types
  implement `FromValue`/`IntoValue`: `i64`, `f64`, `bool`, `String` and `&str`,
  `Vec<u8>` and `&[u8]` (as `bytes`), `()`, `Vec<T>` (any element type,
  e.g. `Vec<Vec<i64>>`, `Vec<(String, i64)>`, `Vec<Option<T>>`), `Option<T>`,
  `Result<T, E>` (as `Result<T, Error>`: a `HostError { kind, message }`
  keeps its kind, any other `E: Display` has kind `"error"`),
  `HashMap<String, T>` / `BTreeMap<String, T>` (as `[str: T]`), and tuples up
  to 4. `&str` and `&[u8]` arguments borrow the script's values for the call
  (async functions take owned arguments). Conversions are charged to the
  fuel meter and checked against `max_memory`. The Sand signature is
  derived from the Rust types, with parameters named `arg1`, `arg2`, ...
  unless `.params(&["x", "y"])` follows. `guarded_function(name, guard, f)`
  checks the capability `guard` computes from (references to) the arguments
  before calling `f`, and returns `Result<R, Error>` (`permission_denied`
  on a denial). `NativeModule::source` may add Sand declarations (types,
  pure-Sand helpers, `native fn` with explicit signatures, which take
  precedence over derived ones and are required for
  `NativeModule::raw_function`). `NativeModule::value_type::<T>()` registers
  `T: ValueType` as the module's value type `T::NAME` (§5.4) and adds `pub
  native type NAME` if the module source does not declare it. `T` must
  implement `Clone + Eq + Hash + Display`, equal values must produce identical
  `Display` output, and `T::parse` must parse that output back into an equal
  value. Set `ValueType::ORDER` to support `Ord` and `ValueType::PACKING` to
  pack values into 16 bytes on the heap. Host functions can accept and return
  any registered value type, including the `time` module types
  (`sandlang::jiff::Timestamp`, `sandlang::jiff::civil::{Date, Time,
  DateTime}`, and `sandlang::time::*`). Using a standard module name
  (`NativeModule::new`), registering a module twice (`Config::module`), or
  registering a function twice panics.
- `Outcome { exit_code: i32 }`; errors are `Error::Compile(diagnostics)`,
  `Error::Runtime { message, trace }` (the run failed with an error whose
  display form is `message`, §10.3), `Error::Internal { message, trace }`,
  `Error::Limit(kind)`, `Error::Interrupted`.
- An internal error (§10.3) is `Error::Internal`. A Rust panic inside the
  engine (a bug in a library or host native, an I/O operation or a prompter)
  does not unwind into the host: it is an internal error. This requires
  `panic = "unwind"` (the default). A session in which an internal error
  happens is reset (§16.3).

### 16.1 Operating-system access and file systems

The crate's features choose what an engine can reach at all:

- `os` (default): the host's file system, network sockets (`net`, `http`),
  and the process's standard input (`Config::inherit_stdin`) and environment
  (when `Config::env` gives none). Without it the engine cannot reach the
  operating system, whatever the policy says: `net` and `http` do not
  exist, the environment is empty unless given, standard input is what the
  host gives (`Config::stdin_bytes`), scripts see a memory file system, and
  the engine builds for `wasm32-unknown-unknown` (§16.2).
- `tls` (implies `os`): TLS for `https` and `net.connect_tls` (rustls with
  ring). The `sand` command (the `sand-cli` package) enables it.

`Config::memory_fs(fs)` makes a `MemFs`, an in-memory file system, the file
system of the engine's scripts (the `fs` module, script imports and the
main file of `run_file`) in any build; without `os`, an engine has an empty
one unless given one. Clones of a `MemFs` share their files, so the host
can prepare files and read back what scripts wrote, and engines can share
one. `MemFs::with_capacity(bytes)` limits its contents (default 64 MiB;
each entry counts 64 bytes more than its name and contents); `write`,
`read`, `create_dir_all`, `remove`, `clear` and `files()` are for the host.
The working directory and module root are then memory paths (default `/`).
The policy decides on these paths as written: a `RulePolicy` keeps each
path rule both canonicalized on the host and normalized lexically, and
compares memory paths with the latter (exactly, also on Windows).

```rust
let fs = MemFs::new();
fs.write("/data/in.txt", "input")?;
let config = Config::new().memory_fs(fs.clone()).policy(Policy::rules(
    RulePolicy::new().allow_read("/data").allow_write("/out")));
// ... run a script; then:
let report = fs.read("/out/report.txt")?;
```

`check_source`, `check_file`, `lint_source` and `lint_file` load
imports like a run (from the engine's file system, under its policy), but
never prompt: imports the policy would prompt for are denied.

### 16.2 The web

Built without `os`, the engine runs as WebAssembly in a JavaScript host
(`wasm32-unknown-unknown` with `wasm-bindgen`). The clock and timers come
from JavaScript; there are no threads, so the compiler's recursive passes
run on the module's stack, linked large enough for the deepest nesting the
parser accepts. A run executes on the calling thread and yields to the
event loop only while it waits (for a timer, I/O or a prompt); hosts run
engines in Web Workers to keep pages responsive and run several in
parallel. The `sandlang-wasm` crate (`wasm/`) is the JavaScript API: an
`Engine` built from a policy file, files, an environment, arguments,
standard input and a time zone, with `onOutput` and `onPrompt` callbacks
(requests the policy does not decide go to `onPrompt`, and are denied
without it), `run`, `check`, `lint`, sessions (`eval(source, name?)`, and
`runTasks`/`stopTasks` to run tasks between inputs), `format` and
`highlight`. The web playground (`site/playground/`) is built on this package.

### 16.3 Sessions

`Engine::session()` returns a `Session`, which runs inputs one at a time
with the semantics of `sand repl` (§17.6) and the engine's configuration
(policy, prompter, limits, streams, host modules, module root and working
directory):

```rust
let mut session = Engine::new(config).session();
session.eval("var hits = 0\nfn hit() -> int { hits += 1\n hits }").await?;   // Ok(None)
assert_eq!(session.eval("hit() + hit()").await?, Some("3".to_string()));
```

- `eval(source) -> Result<Option<String>, Error>` checks, compiles and runs
  one input; `Some` holds the debug form of its value. `eval_as(name,
source)` uses `name` in diagnostics and traces instead of `<input
N>` (e.g. `main.sand` for a program the session continues). After
  `exit(code)`, `eval` returns `Ok(None)` and `exit_code()` returns
  `Some(code)` until the next input; the session remains usable.
- If an input imports a script module whose source file or transitive script
  dependencies changed since the previous load, the session reloads that module.
  Bindings from earlier inputs keep their previously loaded module versions.
- `type_of(expr) -> Result<String, Error>` checks an expression without
  running it or changing the session.
- `run_tests() -> Result<TestReport, Error>` runs the `test` blocks entered so
  far, against the current state.
- `reset()` forgets all inputs. `interrupt_handle()` interrupts the input being
  run (the engine's own handle does not affect sessions).
- `set_wall_time(limit)` replaces the engine's `max_wall_time` for the
  session's later inputs (e.g. a per-input timeout chosen by the host).
- `run_tasks_until(until, on_error) -> Option<T>` runs the tasks earlier
  inputs left running until the future `until` completes, and returns its
  output; front ends call it while waiting for input (§17.6). The
  instruction, CPU time and output limits apply to each call; the wall-time
  limit does not. Permission prompts wait until `until` completes. If a
  task fails (an error, a limit, an interruption), all tasks are cancelled and the
  error goes to `on_error` at once; the call goes on waiting. If a task
  calls `exit`, it returns `None` at once.
  `cancel_tasks()` cancels the tasks left running, without running their
  defers.
- After an internal error (§10.3) in `eval`, `run_tests`,
  `type_of` or `run_tasks_until`, the session is reset as by `reset()`: the
  error's message ends with `" (the session was reset)"`
  (`sandlang::session::RESET_NOTE`), and `was_reset()` returns true until
  the next call. "Always" answers given before are kept.
- `Session::is_complete(source)` tells whether a line-based front end should
  read more lines before evaluating (§17.6).
- `Session: Send`; its futures are `Send`. A session shares the "always"
  answers of its engine (§14.3). Dropping an `eval` future before it
  completes (e.g. on a host timeout) is like interrupting the input; if it
  was still loading imports, the input has no effect.

---

## 17. Tooling

All subcommands exit with code 2, after a message on standard error, for usage
errors: an unknown subcommand or option, a missing file argument, or an option
value that cannot be parsed.

### 17.1 `sand check FILE...`

Parses and type checks; prints diagnostics; exit code 2 on errors or if a file
cannot be read. A `.policy` file is parsed as a policy file (§14.7).

### 17.2 `sand lint FILE...`

Reports warnings (exit code 1 if any, unless `--allow-warnings`; 2 if a file
cannot be read or has errors). Policy files get the lints of §14.7; scripts
these:

| Lint                   | Description                                                                                                                                                                                                                             |
| ---------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `unused-variable`      | local binding or parameter never read (names starting with `_` are exempt)                                                                                                                                                              |
| `unused-import`        | import never referenced                                                                                                                                                                                                                 |
| `unused-function`      | private function or method not reachable from the module's public items, top-level code or tests (see below)                                                                                                                            |
| `unnecessary-var`      | `var` never reassigned (use `let`)                                                                                                                                                                                                      |
| `unused-result`        | a `Result` value is discarded                                                                                                                                                                                                           |
| `no-effect`            | an expression statement that only computes a value which is discarded (`x + 1`, `-3`: §2.3); not the last statement of a block, which is its value                                                                                      |
| `unreachable-code`     | statement after `return`, `break`, `continue`, a call of the prelude's `fail`, `exit` or `todo` (not of a function of the module with that name), or after a `loop` without an exiting `break` (labeled breaks from nested loops count) |
| `unused-label`         | loop label not referenced by `break` or `continue`                                                                                                                                                                                      |
| `naming`               | violates naming conventions (§2.4)                                                                                                                                                                                                      |
| `bool-comparison`      | comparing with `true`/`false` literal                                                                                                                                                                                                   |
| `self-assignment`      | `x = x`                                                                                                                                                                                                                                 |
| `len-zero`             | `x.len() == 0` (use `is_empty()`)                                                                                                                                                                                                       |
| `nan-comparison`       | a comparison with `nan`, which is always false (`!=` is always true); use `is_nan()`                                                                                                                                                    |
| `empty-block`          | empty `if`/`else`/loop body                                                                                                                                                                                                             |
| `todo`                 | call to `todo()`                                                                                                                                                                                                                        |
| `shadowed-variable`    | `let`/`var` binding a name that is already bound, with a different type, by a binding of the same function that is still in scope (see below)                                                                                           |
| `variant-like-binding` | a name starting with an upper-case letter that binds a new variable in a pattern (`match`, `if let`, `while let`, `let`, `for`), usually a mistyped variant without a payload (see below)                                               |
| `unclosed-resource`    | a resource (§7.2) created and never closed: bound with `let`/`var` or used as a temporary, and neither closed by `use` nor handed on (see below)                                                                                        |
| `resource-field`       | a struct field or enum variant holding a resource, in a type that does not implement `Close`                                                                                                                                            |
| `resource-escapes`     | a resource bound with `use`, or received as a parameter, that is kept beyond the call: returned (`use` only), stored, captured by a task, or passed to a function that keeps it                                                         |

`unused-function`: public items are `pub` top-level functions and all
methods of `pub` types. A private function is used only if reachable from
top-level code, a `test` block or a public item, through calls (including
method calls), function values, default arguments and the
arguments of `help` and `members`; a function referenced only by itself or by
unused functions is reported.

`shadowed-variable`: the earlier binding may be a parameter, a `let`/`var`
(including destructuring), a loop variable, or a `match`/`if let`/`while
let`/lambda/comprehension binding, and must still be in scope, so bindings
in sibling blocks (two `if` branches, two loops, two `match` arms) do not
shadow each other. A lambda belongs to its enclosing function, but each
named function and `test` block is checked on its own, so re-binding a
top-level name is not reported.

The resource lints follow resources through each function. A resource is
_created_ by a call that returns one (or a `Result`, option or tuple holding
one) unless its result may come from an argument, and by a literal of a
`Close` type (struct, list, option, ...), which takes over the resources it
is built from. It is _handed on_ when returned, stored (in a field, list,
map, set, global or channel), captured by `spawn`, taken over by a new
resource, or passed to a parameter the callee hands on (calls through
function values and trait dictionaries count as neither keeping nor
returning their arguments). A created resource neither handed on nor bound
with `use` (or `let` directly followed by `defer` of its `close`) is
`unclosed-resource`; a `use`-bound or parameter resource that is handed on
is `resource-escapes` (returning a parameter is allowed). A deliberate
escape is acknowledged with `// lint: allow(resource-escapes)`.

`variant-like-binding`: a bare name in a pattern is a payload-less variant
of the scrutinee's enum if it has one of that name, and otherwise a new
binding matching anything (§8.1), so a misspelled variant (`Emtpy =>`)
silently becomes a catch-all arm. The warning suggests the closest
payload-less variant within a small edit distance (`` `Emtpy` binds a new
variable; did you mean the variant `State.Empty`? ``), otherwise a
lowercase name. Top-level `let` constants in `SCREAMING_SNAKE_CASE` are
exempt. It replaces `naming` for such names.

Parameters of trait and impl methods are exempt from `unused-variable`; the
names of impl methods are checked at the trait.

A lint can be suppressed for the next line with `// lint: allow(name)`, or for
a file with `//! lint: allow(name)`.

Linting checks the file first, with the default limits (§15.3). The lints
may spend 64 units of fuel per byte of source (at least 10,000,000), about
one per syntax tree node and per node a lint examines again; a file that
needs more fails with `source too complex to lint` (exit code 2).

### 17.3 `sand fmt FILE... [--check]`, `sand fmt - [--check]`

Rewrites files in canonical style: 4-space indentation, one statement per
line, spaces around binary operators, no space before `(` in calls, `{` on
the same line, trailing commas in multi-line lists/arguments/fields, at most
one blank line in a row, one blank line around functions with a body,
types, `extend`, traits, impls and tests, line width 100 (calls, lists,
maps and parameter lists that do not fit are broken one element per line,
and method chains that do not fit are broken before each method call),
comments preserved. Trait and impl bodies are laid out like struct bodies,
one method per line, with a blank line around methods with a body. String
literals, including their interpolations, are copied verbatim. Formatting
is idempotent and never changes the parse tree (modulo spans). `sand fmt -`
formats standard input to standard output. With `--check`, nothing is
written or printed, and the exit code is 1 if any input is not formatted.
Exit code 2 if a file cannot be read or parsed. Policy files are formatted
as §14.7 describes. Formatting may spend 64
units of fuel per byte of source (at least 10,000,000), about one per
syntax tree node and layout node examined, per 64 bytes of text measured,
and per byte of output; a file that needs more fails with `source too
complex to format` (exit code 2).

### 17.4 `sand test FILE... [OPTIONS]`

Runs the module's top-level code with output captured, then every `test` block
as a separate task with a fresh view of top-level state _as initialized_. Each
test passes if it completes without an error. Output: one line per test
(`ok name` / `FAIL name: message`) and a summary `N passed, M failed`. Exit
code 1 if any test failed, 2 for compile errors. `sand test` takes the
policy and limit options of `sand run` (§17.5) and, like it, denies what
they do not allow. A test that exceeds a limit fails with the limit's
message.

### 17.5 `sand run FILE [-- ARGS...]`

Options: `--policy=FILE` (a policy file, §14.7: the other options add rules
to it and override its limits; its `default prompt` prompts like
`--prompt`), `--allow-read=PATHS`, `--allow-write=PATHS`, `--allow-net=HOSTS`,
`--deny-net=HOSTS`, `--allow-private-net`, `--allow-listen=ADDRS`,
`--allow-env=NAMES`, `--allow-stdin`, `--allow-timezone`, `--allow-all`,
`--prompt` (prompt on the terminal for unmatched requests), `--max-memory=BYTES`
(suffixes K, M, G), `--max-time=DURATION`, `--max-cpu=DURATION`,
`--max-instructions=N`, `--max-compile-fuel=N`, `--seed=N`. Arguments after `--`, and positional
arguments after `FILE`, are passed to the script. Exit code: the script's exit
code (codes outside `0..=255` exit with 255), 1 for a runtime error or `?`
failure, 2 for compile errors, 3 for limit violations. If the policy allows
`TimeZone`, `time.local_zone()` returns the system time zone.

`--prompt` asks on the controlling terminal (`/dev/tty`; the console on
Windows), never on the script's standard streams, so piped input cannot
answer; without a terminal, prompted requests are denied. Answers: `y`
(allow once), `a` (always), `n` or an empty line (deny once), `N` (never);
end of input denies, and an unrecognized answer is asked again up to three
times before denying. Control characters in the request are escaped, as
are Unicode format characters (such as bidirectional overrides), line and
paragraph separators, spaces other than U+0020 and combining marks. Before
each question the terminal's rendition is reset (soft terminal reset and
SGR reset, on a fresh line) and a marker line is shown, so output the
script left concealed or styled cannot hide or imitate the question.

Unless `--allow-private-net` or `--allow-all` is given, private and
internal network destinations are denied (§14.6): `--allow-net=*` does not
reach `localhost` or `10.0.0.5`, but `--allow-net=localhost:8080`,
`--allow-net=127.0.0.1` or `--allow-net=10.0.0.0/8` do. Malformed network
patterns are rejected.

### 17.6 `sand repl [OPTIONS]`

Interactive read-eval-print loop, taking the options of `sand run` (§17.5).
It runs a session (§16.3): each input is type checked in the context of the
previous inputs, then compiled and run **exactly once**; state persists
between inputs.

- An input is any mix of what a module may contain (§3.1), including
  `import`, `trait`, `impl` and `test` blocks. Its names join the session's
  single module scope.
- If the input's last item is an expression statement, its value is printed
  in debug form (§10.9), unless it has type `()` or the input ends with `;`.
- Top-level `let`, `var` and `use` bindings are globals that later inputs may
  read (§7.2); a `var` declared by an earlier input may be assigned. Within one
  input, names must be unique as in a module; a later input may define any
  name again (as any kind of item), which shadows the earlier definition for
  later inputs.
  Code checked earlier keeps referring to what it referred to then. Methods
  (`extend`) and impls (§4.9) of a later input likewise replace those of
  earlier inputs with the same name or for the same type.
- A top-level variable's type must be inferred within the input that declares
  it.
- An input that fails to compile has no effect. An input that fails with an
  error (§10.3), exceeds a limit or is interrupted keeps its declarations and
  every effect that happened before the failure, including assignments to
  globals; globals it had not initialized yet remain uninitialized (reading
  them fails, §3.1).
- A top-level `return` ends the input. The value it returns is shown like the
  value of a final expression statement, unless it is `()` or the input ends
  with `;`; returning `Err(err)` fails the input with `err`.
- Tasks spawned by an input keep running during later inputs and while the
  REPL waits for input, and can be awaited by later inputs. An error in any
  task fails the input being run (or, between inputs, is reported). After a
  failed input or task, all tasks are cancelled. While the REPL waits, the
  tasks' output appears above the input being edited (on a terminal, on
  standard output whichever stream it was written to) or starts on a new
  line followed by the prompt again; their permission prompts wait until
  an input has been entered.
- `exit(code)` ends the session with exit code `code`.
- The instruction, CPU time, wall time and output limits apply to each
  input; the memory limit to the session's heap. Between inputs, the
  instruction, CPU time and output limits (not wall time) apply to the tasks'
  work during each wait for input. "Always" answers apply to the rest of the session.
- After an internal error (§16) the session is reset, as by `:reset`, and
  the error says so.
- `test` blocks are checked but not run when entered.

Lines are read until the input is complete: while brackets, a string or a
block comment are open, or the last token cannot end a statement (a binary
operator, `=`, `,`, `.`, `:`, `=>`, `->`, `else`, ...), or the last line is
a doc comment, the REPL prompts with `...` for more. A command line is
always complete. Ctrl-C interrupts the input being run (the session
continues), discards a partially entered input, or, at an empty prompt,
cancels the tasks left running. End of input (Ctrl-D, on an empty input in
the line editor) ends the session, running any partial input first. Results
go to standard output, diagnostics and errors to standard error. Scripts
read standard input through the REPL (`--allow-stdin`): while an input runs,
if a script waits for standard input and nothing else can run, the next line
goes to the script (without the line editor), and Ctrl-D ends that read.
Between inputs, lines go to the REPL and a task's read waits.

When standard input and output are a terminal, inputs are edited with a line
editor. An incomplete input continues on `...` lines (Enter on an incomplete
input starts a new line), and is edited, recalled and kept in the history as
a whole. Keys (Emacs style):

| Keys                                                     | Effect                                               |
| -------------------------------------------------------- | ---------------------------------------------------- |
| Left, Right, Ctrl-B, Ctrl-F                              | move by character                                    |
| Alt-Left, Alt-Right, Ctrl-Left, Ctrl-Right, Alt-B, Alt-F | move by word (macOS: Option-Left, Option-Right)      |
| Home, End, Ctrl-A, Ctrl-E, Super-Left, Super-Right       | start or end of the line (macOS: Command-Left/Right) |
| Ctrl-Home, Ctrl-End                                      | start or end of the input                            |
| Up, Down                                                 | previous or next line of the input, else history     |
| Ctrl-R                                                   | search the history                                   |
| Ctrl-W, Alt-Backspace, Alt-D, Ctrl-U, Ctrl-K             | cut a word back or forward, to the start or end      |
| Ctrl-Y, Ctrl-Z, Ctrl-G                                   | paste the cut text, undo, redo                       |
| Ctrl-L                                                   | clear the screen                                     |

The history holds the last 1000 inputs (commands included) in the file
named by `SAND_HISTORY` (not kept if empty), by default `.sand_history` in
the home directory.

Commands (on a line of their own):

| Command       | Effect                                                                                                                                                 |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `:help`       | show the commands                                                                                                                                      |
| `:type EXPR`  | show the type of an expression without running it (a generic function as `fn<T: Ord>([T]) -> T?`)                                                      |
| `:test`       | run the `test` blocks entered so far, against the current state (the last block of each name; effects persist), and print a report as `sand test` does |
| `:reset`      | forget all inputs and start a fresh session ("always" answers are kept)                                                                                |
| `:quit`, `:q` | end the session (exit code 0)                                                                                                                          |

---

## 18. Diagnostics

Each diagnostic contains a severity (error/warning), a code, a message, and a primary
span (file, line, column) with optional notes. Rendered form:

```text
error[E0308]: type mismatch: expected int, found str
  --> main.sand:3:9
   |
 3 | let x: int = "hi"
   |              ^^^^
```

When a type mismatch involves distinct types of the same name (from two
modules, or a type a REPL input defined again), each is followed by where it
is defined: `expected Point (defined in input 3), found Point (defined in
input 1)`, `... (defined in module geo)`; an optional such type is shown as
`Option<Point (defined in ...)>`.

The checker continues after errors where possible, reporting at most 50
(§15.3). Parse errors stop checking of the affected module.
