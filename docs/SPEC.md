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

`help` displays doc comments (§12.15). A doc comment on an `extend` block
documents a built-in type. A `///` comment must appear on its own line before
the item it documents; a `///` comment after code on the same line is a regular
line comment. The lexer strips trailing whitespace from doc comment lines. A
`//!` comment after the first item is a syntax error.

### 2.3 Line termination

Newlines are significant. The lexer converts a newline into a statement
terminator (`;`) if the last token on the line is:

- an identifier, a keyword after `.` (`x.or`), or the keyword `self`, `Self`, `true`, `false`, `break`, `continue` or `return`
- a literal (number, duration, string, bytes)
- a closing `)`, `]` or `}`
- the postfix operator `?`
- `..` or `..=`, which may end an open range (`let rest = 1..`)

and the newline is **not** inside a `(...)` or `[...]` group. Inside `{...}`
newlines are significant again, even inside parentheses.

The lexer does not insert a terminator if the next non-blank line starts with `.`,
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

`pub` may precede top-level `fn`, `struct`, `enum`, `trait`, `type`, `let`, and
`var` declarations, struct fields, and methods in `struct`, `enum`, and
`extend` blocks. Other modules cannot reference a private item, access or match
a private field, call a private method, or construct a struct that has a
private field (with `T(...)` or `json.decode`). Value formatting and
`json.encode` include all struct fields regardless of visibility. Enum variants
and the methods of a public trait are always public; `impl` blocks do not have
visibility modifiers (§4.9). Importing modules can read a `pub var`, but only
the declaring module can assign to it.

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
- Type parameters are listed in `<...>`. A `:` after a type parameter specifies
  the traits (§4.9) that its type arguments must implement, including built-in
  traits (§5.6), module-qualified traits, and generic traits:
  `T: Ord + Describe`, `T: geo.Shape`, `T: Into<str>`. A trailing `where`
  clause (`fn sum(self) -> T where T: Num`) also specifies trait bounds, and
  can constrain the type parameters of an enclosing generic `extend` block,
  struct, or enum for that method.
- A type parameter marked `reflect` (`fn load<reflect T: Decode>(text: str)
  -> Result<[T]>`) receives a runtime type description of its type argument. A
  function can forward `T` (or a composite type containing `T`, such as `[T]`)
  to another `reflect` type parameter, such as `json.decode<T>`. Every type
  parameter that appears in a `reflect` type argument must itself be declared
  `reflect` on the enclosing function. If recursive calls nest constructed type
  descriptions more than 256 levels deep, execution aborts with kind `"limit"`.
  Local functions, trait methods, and `impl` methods cannot declare `reflect`
  type parameters.
- Functions may be declared inside blocks; such local functions may capture
  enclosing variables and may call themselves recursively. Local functions
  declared consecutively in the same block may call each other. Their type
  parameters can only require traits that do not declare methods (such as `Eq`
  or `Hash`).

### 4.2 Structs

```sand
pub struct Point {
    pub x: float
    pub y: float

    /// Distance from the origin.
    pub fn norm(self) -> float {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    pub fn origin() -> Point { Point(x: 0.0, y: 0.0) }
}

struct Counter {
    name: str
    pub var count: int = 0
}

pub struct Port {
    n: int

    /// Fails if `n` is not a TCP port number.
    pub fn init(n: int) -> Result<Port> {
        if n < 1 or n > 65535 { return Err(Error("no such port: \(n)", kind: "invalid")) }
        Ok(Self(n: n))
    }
}
```

- Fields are separated by newlines or commas and are immutable unless
  declared `var`. If a construction (§6.4) omits a field, the engine evaluates
  the field's default expression. A field default can use traits without
  methods from the struct's type parameter bounds (`s: Set<T> = Set([])` with
  `T: Hash`), but cannot use traits that declare methods (§4.9). Fields and
  methods are private to the declaring module unless marked `pub` (§3.3).
- A struct can declare a static method `init` that returns `Self` or
  `Result<Self, E>`. If a struct declares `init`, `T(args)` calls `init` and
  returns its result (for example, `Port(80)` has type `Result<Port>`). Inside
  `init`, `T(args)` and `Self(args)` construct the struct field by field. Native
  modules can also declare `native fn init` on built-in or opaque types (such as
  `Set([1, 2])` and `regex.Regex(pattern)`). Enums and traits cannot declare
  `init`, and expressions cannot reference `T.init` as a member. `init` must
  apply to all instances of the type: an `extend` block that declares `init`
  cannot specialize the target's type parameters, and `init` cannot add trait
  bounds beyond those declared on the type.
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
  payload fields, which are all named (`radius: float`) or all positional
  (`float`).
- Variants are referred to as `Enum.Variant` in expressions, e.g.
  `Shape.Circle(1.0)` or `Shape.Circle(radius: 1.0)`. In a pattern, the enum
  prefix is optional if the scrutinee type is known, and a sub-pattern can
  match a named payload field by name: `Rect(h: height, ..)`.
- `Option` and `Result` variants (`Some`, `None`, `Ok`, `Err`) are always in
  scope unqualified.
- Enum values are immutable.

### 4.4 Extending types

```sand
extend Point {
    fn scale(self, k: float) -> Point { Point(x: self.x * k, y: self.y * k) }
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
not be recursive, and their type parameters cannot require traits.

### 4.6 Top-level variables

```sand
let GREETING = "hi"
pub let LIMITS = [1, 2, 3]
var counter = 0
```

Top-level `let` and `var` follow the same rules as local bindings; their types
must be fully inferred from their initializer and module-level uses.

### 4.7 Native declarations

The prelude, standard library, and host native modules can declare `native`
functions and methods without a body. The module provides the Rust
implementation, and the engine verifies that the declared Sand signature
matches the Rust function signature (§16.3). User scripts cannot declare
`native` functions.

```sand
pub native fn parse(text: str) -> Result<Json>
```

A native function receives the runtime description of each `reflect` type
parameter (§4.1) as a `Reflect<N>` argument (§16.3).

Each type parameter on a `native fn` declaration must declare the built-in
traits that the Rust function requires (§16.3).

Native modules register Rust-bound opaque types (§5.4), enums, and structs
directly from Rust rather than declaring them in Sand source.

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
can call the methods of the traits that its type parameters require.

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
  a **default** method that applies to any `impl` that does not override it.
  Methods whose first parameter is `self` are instance methods; all other
  methods are static methods. Trait methods cannot use `pub` (they share the
  visibility of the trait) or `native`.
- Within a trait declaration, `Self` represents the implementing type. Because
  `Self` implements the trait and its supertraits, default methods can call any
  method of the trait (`self.describe()`, `Self.empty()`) or of its supertraits.
- A trait can declare type parameters (`trait Into<T>`), which cannot have
  trait bounds. Trait methods can declare their own type parameters with trait
  bounds, and `where` clauses on a trait method can constrain only the method's
  own type parameters.
- Default values on trait method parameters must be constants (literals, `[]`,
  `[:]`, or `None`).
- **Supertraits** follow a `:`: `trait Shape: Describe + Eq`. Any type parameter
  bounded by `Shape` also satisfies `Describe` and `Eq`. Supertrait hierarchies
  cannot contain cycles.
- Traits share the type namespace. In a trait bound after `:`, a module-local
  type does not shadow a prelude trait of the same name (`struct Hash` does not
  shadow `T: Hash`). User code cannot redefine or implement built-in traits
  (§5.6).

**Impls.** An `impl` block (`impl<P...> Trait<Args> for Type where ... { methods }`)
implements a trait for a target type.

- The target type must have a type constructor: a struct, an enum (including
  `Option` and `Result`), `Error`, an opaque type, a tuple (each arity is a
  distinct constructor), or a built-in type (`int`, `float`, `bool`, `str`,
  `bytes`, `duration`, `()`, `Range`, `[T]`, `[K: V]`, `Set<T>`, `Task<T>`, or
  `Channel<T>`). Function types and bare type parameters
  (`impl<T> Trait for T`) are not valid target types. Target type arguments can
  be concrete types (`impl Describe for [int]`).
- Every type parameter of an `impl` block must appear in the target type or in
  the trait arguments. Trait bounds declared on the `impl` block (in `<...>` or
  in a `where` clause) must hold at every use of the `impl` (for example,
  `impl<T: Describe> Describe for [T]` implements `Describe` for `[T]` if `T`
  implements `Describe`).
- An `impl` block must define every required method of the trait, may override
  default methods, and cannot declare methods that are not in the trait. Each
  method signature must match the trait declaration after substituting the
  target type for `Self` and the `impl` trait arguments for the trait's type
  parameters (method type parameters may be renamed if they declare equivalent
  trait bounds). Methods in an `impl` block cannot specify `pub`, parameter
  defaults (the trait's defaults apply), or bounds on the `impl` block's type
  parameters. Within an `impl` block, `Self` is an alias for the target type.
- **Supertraits:** the target type must implement every supertrait of the
  implemented trait under the `impl` block's trait bounds.
- **Coherence:** a trait can have at most one implementation for a given type
  constructor and unifiable trait arguments. Two `impl` blocks for the same
  trait and type constructor conflict if their trait arguments unify:
  `impl Into<str> for int` and `impl Into<float> for int` do not conflict,
  whereas `impl Describe for [int]` and `impl Describe for [str]` conflict.
- **Orphan rule:** an `impl` block must appear in the module that declares the
  trait or the module that declares the target type. Because the prelude
  declares built-in types, other modules can implement only their own traits for
  built-in types.
- Implementations are unnamed and always visible once their declaring module is
  loaded.

**Trait bounds.** A type argument passed to a parameter bounded by
`T: Trait<Args>` must satisfy that bound. A concrete type satisfies a bound
through a matching `impl` (explicit or compiler-derived, §5.6) if the type
arguments satisfy all bounds required by that `impl` recursively (for example,
`[[Point]]: Describe` requires `[Point]: Describe`, which requires
`Point: Describe`). A type parameter satisfies its declared bounds and their
supertraits, and `Self` satisfies the enclosing trait and its supertraits. The
type checker verifies trait bounds at every instantiation and reports any
unmet bound (for example, `str does not implement `Describe` (required for
[str] to implement `Describe`)`).

Functions, methods, `where` clauses, `impl` blocks, structs, and enums can
declare trait bounds on their type parameters. Methods on a struct or enum
(including methods in `extend` blocks) inherit the type parameter bounds of the
type. A struct or enum also infers any bounds that its field or variant payload
types require of its type parameters: given `struct Bag<T: Hash>`,
`struct Outer<U> { b: Bag<U> }` infers `U: Hash`, and `help(Outer)` displays
`struct Outer<U: Hash>`. Because `Set` elements and map keys require `Hash`, a
field of type `Set<[int]>` is rejected at declaration time. The type checker
verifies type constructor bounds on constructions, enum variants, method
calls, struct fields, enum payloads, and `reflect` type arguments; other type
annotations are not checked.

A trait that does not declare methods in itself or its supertraits
(`trait Marker {}`, and all built-in traits except `Num`) does not pass a
runtime method dictionary to generic functions.

**Method calls.** For a call `e.m(args)`:

- If the receiver type is a type parameter, the type checker resolves `m` among
  the parameter's declared traits and their supertraits.
- For any other receiver type, after checking function-typed fields and
  inherent methods (§9), the type checker resolves `m` among all traits that
  are public or declared in the current module and implement the receiver's type
  constructor. The caller does not need to import the trait.

If multiple traits provide `m`, the call is rejected as ambiguous. On concrete
receiver types, traits from user or library modules take precedence over prelude
traits: given `impl Monoid for int`, `int.zero()` resolves to `Monoid.zero`
rather than `Num.zero`. Callers can also invoke any trait method using
qualified syntax:

```sand
Describe.describe(p)          // Self is inferred from the arguments
geo.Shape.area(c)             // a module-qualified trait
Point.describe(p)             // through the type
T.empty()                     // a static method through a type parameter
Self.empty()                  // in traits and impls
let m: [str] = Monoid.empty() // Self is inferred from the expected type
```

Static trait methods cannot be called on a receiver value. For a generic trait,
the type checker infers the trait's type arguments from the receiver's type
parameter bounds, from the unique matching `impl` for the receiver's type
constructor, or from the expected return type; if multiple `impl` blocks remain
applicable, a type annotation is required.

Trait methods (`Point.describe`, `Describe.describe`, `T.empty`) and generic
functions with trait bounds can be used as first-class function values; the
type checker resolves their trait bounds at the expression that creates the
function value.

Traits are not types: Sand does not have trait objects (`dyn Trait`). Use an
enum to store values of different concrete types in a single collection.

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
`fn() -> never` is expected are type errors. `never` implements only traits that
do not declare methods (§4.9), such as `Eq` and `Hash`, because a trait method
such as `T.zero()` can be called without a value of type `T`.

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
pub struct Error { pub message: str, pub kind: str = "error" }
```

`Error.kind` is a short machine-readable tag. Standard kinds are:
`"error"`, `"not_found"`, `"permission_denied"`, `"already_exists"`, `"io"`,
`"timeout"`, `"parse"`, `"invalid"`, `"http"`, `"closed"`, `"limit"`.

The prelude also defines the lazy iterator struct `Iter<T>` and the trait
`Iterator<T>` (§12.13).

Standard library and host modules define additional nominal types (such as
`json.Json`, `http.Response`, and `net.Conn`). An _opaque_ type binds a
registered Rust type (§16.3); it does not have fields, and only its `init`
(§4.2) or functions and methods in its module can construct its values. Opaque
value types (such as `time.PlainDate`) are immutable, compare and hash by
value, can implement `Ord`, and format as text (`2024-02-29`). Opaque object
types (such as `net.Conn`) compare by identity and format as `<Type>`; tasks
and channels format as `<task>` and `<channel>`.

### 5.5 Type equality and subtyping

Apart from `never`, there is no subtyping and there are no implicit conversions.
In particular `int` and `float` never convert implicitly: `1 + 2.0` is a type
error. Use `x.to_float()` or `f.to_int()`.

### 5.6 Built-in traits

The prelude declares six built-in traits that govern equality, ordering,
hashing, JSON serialization, and numeric arithmetic:

| Trait    | Supertraits    | Implemented by                                                                                                                                                                              |
| -------- | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Eq`     |                | every type except function types (and types containing them)                                                                                                                                |
| `Ord`    | `Eq`           | `int`, `float`, `bool`, `str`, `bytes`, `duration`, `()`, ordered value types (§5.4), and tuples, lists, `Option`, structs and enums all of whose components are `Ord`                      |
| `Hash`   | `Eq`           | `int`, `bool`, `str`, `bytes`, `duration`, `()`, `Range`, value types (§5.4), and tuples, `Option`, `Result`, enums and structs **without `var` fields** all of whose components are `Hash` |
| `Encode` |                | every type except function types, `Task`, `Channel`, native objects (§5.4), and types containing them                                                                                       |
| `Decode` | `Encode`       | every `Encode` type except structs that declare `init` (§4.2) and types containing them                                                                                                     |
| `Num`    | `Ord + Encode` | `int`, `float`                                                                                                                                                                              |

The compiler automatically derives `Eq`, `Ord`, `Hash`, `Encode`, and `Decode`
from a type's components: a struct or enum implements a derived trait if every
field and variant payload implements it, and a generic type requires the trait
on each type parameter used by those components. For example,
`struct Pair<A, B> { a: A, b: B }` implements `Hash` if `A` and `B` implement
`Hash`, and `help(Pair)` displays `Hash where A: Hash, B: Hash`. `==` and `!=`
require `Eq`, `<`, `<=`, `>`, and `>=` require `Ord`, map keys and set elements
require `Hash`, `json.encode` requires `Encode`, `json.decode` requires
`Decode`, and arithmetic operators on type parameters require `Num`. Only the
prelude implements `Num`.

Built-in traits can be used as bounds on type parameters, supertraits
(`trait Keyed: Hash { ... }`), and `impl` blocks in the same way as
user-defined traits (§4.9). `Num` declares the static methods `zero()` and
`one()` for the additive and multiplicative identities (`0`/`1` for `int` and
`0.0`/`1.0` for `float`), which allows generic functions to fold empty
collections:

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

### 6.4 Construction

```sand
Point(x: 1.0, y: 2.0)
Point(1.0, 2.0)               // fields in declaration order
Point(x: 5.0, ..p)            // copy the other fields from p
geo.Point(x: 0.0, y: 0.0)     // imported type
Set([1, 2])                   // a type that declares `init` (§4.2)
```

`T(args)` constructs a value of type `T`. If `T` declares `init` (§4.2),
`T(args)` calls `init`. Otherwise, positional arguments populate fields in
declaration order, followed by named arguments (§6.6). A field-by-field
construction must provide every field that does not have a default, either as
an argument or via a trailing `..base` argument. `T(args)` cannot use `..base`
if `T` declares `init`.

Used as a value, a type name is a constructor function (for example,
`lists.map(Set)`) that takes all `init` parameters or all struct fields in
declaration order, ignoring defaults. `str` formats it as `<fn T>`, and stack
traces display `init` frames as `T`. A variant with a payload can also be used
as a function value (`xs.map(Some)`).

### 6.5 Operators

**Arithmetic** `+ - * / % **` and unary `-` require both operands to have the
same `Num` type, with these additions:

- `str + str`, `bytes + bytes`, `[T] + [T]` concatenate (producing new values).
- If the operand type of `+` is not yet known (`let add = (a, b) => a + b`), the
  type checker verifies that the inferred operand type is a `Num` type,
  `duration`, `str`, `bytes`, or a list (for example, `add(true, false)` is a
  compile error).
- `duration + duration`, `duration - duration`, `-duration`,
  `duration * k`, `k * duration` and `duration / k` produce `duration`,
  where `k` is any `Num` (a type parameter `k: T` requires `T: Num`);
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
Rect(h: p, ..)        named payload field; `..` ignores the other fields
Point(x, y: 0)        struct
Point(x, ..)
[]  [a, b]  [first, ..rest]  [.., last]   list patterns
p1 | p2               alternatives (must bind the same names with the same types)
```

A binding pattern `name` that coincides with a payload-less variant of the
scrutinee's enum type refers to that variant (`Empty` for a `Shape`, §4.3). A
capitalized name that is not such a variant is a binding, which the linter
reports as a likely mistyped variant (`variant-like-binding`, §17.2). In a
struct or variant pattern, positional sub-patterns match fields in declaration
order, followed by named sub-patterns. The pattern must either match every
field or end with `..`. Struct patterns match fields directly even if the
struct declares `init`. A pattern binds each name at most once.

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
  trait method (§4.9; for a type parameter, from its traits), and the
  universal methods `to_str() -> str` (display form) and `debug() -> str`
  (debug form).
- Integer and float literals have types `int` and `float` respectively; there
  is no literal polymorphism.
- At the end of checking a function, every type variable must be resolved,
  except those bound to the function's own type parameters. Unresolved
  variables are errors (`cannot infer type`).
- The type checker verifies trait bounds at each generic instantiation, or at
  the end of function type inference if a type argument contains inference
  variables at the call site.
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
The arguments of a construction are evaluated in source order, then `..base`,
then the defaults of the remaining fields in declaration order. For assignment
`a[i] = e`, `a` and `i` are evaluated before `e`; for compound assignment
`a[i] += e`, `a` and `i` are evaluated once.

### 10.3 Errors

A run **fails with an error** when the script cannot go on. The run ends (all
tasks), or in a session (§16.6, §17.6) the input does, with a runtime error:
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
is reset (§16.6).

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
| `Point(x: 1.0)`                     | `Point(x: 1.0)`                     | same                                                                            |
| `Circle(2.0)`                       | `Circle(2.0)`                       | same                                                                            |
| `Shape.Rect(w: 1.0, h: 2.0)`        | `Rect(w: 1.0, h: 2.0)`              | same                                                                            |
| `Error`                             | `<kind>: <message>`                 | `Error(message: "...", kind: "...")`                                            |
| function                            | `<fn name>`                         | same                                                                            |
| task / channel                      | `<task>` / `<channel>`              | same                                                                            |
| `0..3`                              | `0..3`                              | same                                                                            |
| `time.PlainDate(2024, 2, 29)?`      | `2024-02-29`                        | same                                                                            |
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

### 11.4 Channels

`Channel(capacity: int = 0)` creates a `Channel<T>` that holds at most
`capacity` queued messages. If `capacity` is `0`, the channel is unbounded. A
negative capacity fails with kind `"invalid"`.

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

`Set(items: [T] = [])` creates a set of the distinct elements of `items`.
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
for x in Countdown(n: 3) { print(x) }          // 3, 2, 1
print(Countdown(n: 3).iter().map(x => x * 2).to_list())  // [6, 4, 2]
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
| `Iter.from_fn(f: fn() -> T?)`                             | the results of `f` until it returns `None` (also `Iter(next: f)`)    |
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
unless stated otherwise, a denial is `Err(Error(..., kind: "permission_denied"))`.

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

`struct Metadata { pub size: int, pub is_file: bool, pub is_dir: bool, pub modified: time.Instant? }`.

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
instead of the host's (§16.4; always without the `os` feature). Paths are
absolute from its root `/` (the default working directory and module root)
and normalized lexically. It has no symlinks, hard links or permissions,
names are case sensitive everywhere, and a write that would exceed its
capacity fails with kind `"limit"`. Operations complete at once rather than
off the VM thread.

### 13.4 `http`

`http` and `net` (§13.10) need operating-system access: without the `os`
feature (§16.4) they do not exist, and importing them is a compile error
saying so.

Headers are lists of `(name, value)` pairs, in order; a header sent several
times (such as `Set-Cookie`) occurs several times. Names of received
headers are lower-cased.

```sand
pub struct Response { pub status: int, pub headers: [(str, str)], pub body: bytes, pub url: str }
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
pub struct Stream { pub status: int, pub headers: [(str, str)], pub body: Body, pub url: str }
pub fn upload(method: str, url: str, headers: [(str, str)] = [], timeout: duration = 30s) -> Result<Upload>
upload.finish() -> Result<Stream>
```

`get`, `post` and `request` read the whole response. `post_json`, `put_json`,
`patch_json` and `request_json` send `json.encode(value)` as the body, with
`Content-Type: application/json` unless `headers` set a content type. Response
methods: `text() -> Result<str>`, `json() -> Result<json.Json>`, `ok() -> bool`
(200..=299), `header(name: str) -> str?` (case-insensitive; the first value),
`header_values(name: str) -> [str]`. HTTP/1.1 over TCP; `https` uses TLS with
the `tls` feature (default). Up to 10 redirects (301, 302, 303, 307, 308)
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
pub struct Request { pub method: str, pub path: str, pub query: str, pub headers: [(str, str)], pub body: Body, pub conn: net.Conn }
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

`conn` is the client connection from which `request.body` reads. A handler can
use `conn` directly between body reads (reading from `conn` consumes bytes
after the bytes already read by `body`), and closing `conn` or `request` ends
the body. `respond` writes a response with a `content-length` header (omitting
the body for `HEAD` requests and bodyless status codes) to `conn`, and then
closes `request`. `respond_stream` returns a `Reply` (`io.Writer`) that frames
the response body in the same way as `Upload` (or terminates the body by
closing the connection for HTTP/1.0 clients without `content-length`); the
server sends the response head on the first write or `finish`, and closing the
`Reply` finishes the response. A script can also write raw HTTP framing
directly to `conn` and close it.

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

On the web (§16.5), `http` is a client over the browser's `fetch` with the
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
`vars() -> [str: str]` (sorted by name; capability `Env("*")`; a denial yields an empty map).

### 13.7 `json`

```sand
pub enum Json { Null, Bool(bool), Int(int), Float(float), Str(str), Arr([Json]), Obj([str: Json]) }
pub native fn parse(text: str) -> Result<Json>
pub native fn encode<T: Encode>(value: T, indent: int = 0) -> str
pub fn decode<reflect T: Decode>(text: str) -> Result<T>
pub native fn to_json<T: Encode>(value: T) -> Json
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

`encode` and `to_json` convert values structurally: `int` → number (`Int`),
`float` → number (`Float`), `str` → string, `bool`, `()` → `null`,
lists/sets/tuples → arrays, maps → objects, structs → objects keyed by field
name, `None` → `null`, `Some(x)` → `x`; other enums: a unit variant →
`"Variant"`, otherwise `{"Variant": [payload...]}` (or an object for named
payloads); `bytes` → base64 string; `duration` → nanoseconds; `a..b` →
`{"start": a, "end": b}` and `a..=b` → `{"start": a, "end": b, "inclusive":
true}`; value types (§5.4) → their text form (`"2024-02-29"`); `Json` → itself.
Map keys are formatted as JSON object keys: `str` as itself, `int`, `duration`,
and `bool` as their text representation, a unit enum variant as its variant
name, and a value type as its text form. Any other map key type aborts with
kind `"invalid"`.

`json!` (§6.13) builds a `Json` from JSON syntax with embedded Sand values.

`decode<T>` is the inverse of `encode<T>`. Decoding a non-empty map with an
unsupported key type fails with kind `"invalid"`. If a JSON object omits a
struct field, `decode` uses the field's declared default, or `None` if the
field is an `Option` without a default. Constant defaults (constructed from
literals, `int` ranges, tuples, lists, maps, `Set([...])`, enum variants, and
structs constructed field by field, up to 65,536 nodes and 64 nesting levels)
are constructed during decoding; non-constant defaults (such as `id: str = uuid.v4()`) are
evaluated after the entire input is parsed, inner structs before outer structs
and fields in declaration order. Any other missing field or type mismatch fails
with kind `"parse"` and the JSON path of the error (such as `$.items[2].name:
expected str`, with type names truncated at 256 bytes with `…`). Parsing is
limited to a nesting depth of 512.

### 13.8 `log`

```sand
pub enum Level { Debug, Info, Warn, Error }
```

`debug`, `info`, `warn`, and `error` emit structured log records with signature
`(msg: str, fields: [str: str] = [:])`. `set_level(level: Level)` sets the
minimum active script log level (default `Level.Info`, unless configured by the
host; see §16). `disable()` disables script logging until the next `set_level`
call.

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
-> [T]`, `bytes(n: int) -> bytes`, `seed(s: int)`. `Rng(seed: int)` creates a
generator independent of the engine's, with the same methods except `seed`.

### 13.12 `regex`

```sand
regex.Regex(pattern: str) -> Result<Regex>
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
pub struct Match { pub text: str, pub start: int, pub end: int }
pub struct Captures { text: str, ... }
Captures: get(i: int) -> Match?, name(n: str) -> Match?, len() -> int, text(i: int) -> str?
```

`Captures` stores the searched text and the start and end byte offsets of each
capture group (`-1` if a group did not match), and its methods slice the
matched text.
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

- `PlainDate(year, month, day)`, `PlainTime(hour, minute = 0, second = 0, millisecond = 0, microsecond = 0, nanosecond = 0)`, `PlainDateTime(year, month, day, hour = 0, ...)`, `PlainYearMonth(year, month)`, and `PlainMonthDay(month, day)` validate their components and return `Result<T>` (error kind `"invalid"`).
- `Span(years = 0, months = 0, weeks = 0, days = 0, hours = 0, ..., nanoseconds = 0)` fails with kind `"invalid"` if units have mixed signs.
- `TimeZone(name) -> Result<TimeZone>` accepts an IANA name (case-insensitive), `UTC`, or a whole-minute offset within `±23:59` (`+00:00` returns `UTC`), and returns an error of kind `"not_found"` otherwise.
- `Instant.from_epoch_seconds`, `from_epoch_millis`, `from_epoch_nanos`; constants `UTC` and `MIDNIGHT`.

Clocks:

- `now() -> Instant` returns the current wall-clock time.
- `local_zone() -> TimeZone` returns the configured host time zone (§16) if the policy permits `TimeZone`, or `UTC` if denied.
- `now_zoned(zone = local_zone()) -> ZonedDateTime` and `today(zone = local_zone()) -> PlainDate`.
- `Stopwatch()` starts a monotonic timer; `sw.elapsed() -> duration` returns the elapsed time.

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
    pub scheme: str, pub userinfo: str?, pub host: str?, pub port: int?,
    pub path: str, pub query: str?, pub fragment: str?
}
pub native fn parse(text: str) -> Result<Uri>          // RFC 3986
pub native fn encode_component(s: str) -> str          // percent-encodes all but unreserved
pub native fn encode_path(s: str) -> str               // also keeps "/"
pub native fn decode_component(s: str) -> Result<str>
pub native fn parse_query(q: str) -> [(str, str)]      // '+' decodes to space
pub fn build_query(pairs: [(str, str)]) -> str
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
  cannot be resolved is not followed (§13.3). Memory file system paths (§16.4)
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
| `Net(host, port, tls)`      | outgoing TCP and TLS connections (`net`)   |
| `Http(method, url)`         | each `http` request and redirect hop       |
| `NetAddr(host, addr, tls)`  | each address `host` resolved to (§14.6)    |
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
prompter exists, the engine denies the request. An "Always" decision
persists for the engine lifetime across all runs and compilations. A "Once"
decision applies to a single request.

`Import` requests are determined during compilation by `check` and `lint`,
once per script module, depth-first in import order, with the module's
resolved absolute path. Standard and host modules are not script modules:
importing them makes no request.

### 14.4 Effects of a denial

- I/O functions return `Err` with kind `"permission_denied"`.
- A denied environment variable is treated as unset.
- `print` and `eprint` fail (kind `permission_denied`) with `permission denied: stdout` / `stderr`
- writes to `io.stdout` and `io.stderr` fail with kind `"permission_denied"`.
- A denied `Import` raises a compile-time error.

### 14.5 Rule policies

`Config::policy(RulePolicy)` sets the rules that decide the script's
requests, and `Config::custom_policy(CustomPolicy)` replaces them with a
host-defined policy. The default is `RulePolicy::standard()`.

| Constructor               | Rules                                                             |
| ------------------------- | ----------------------------------------------------------------- |
| `RulePolicy::default()`   | none                                                              |
| `RulePolicy::standard()`  | `allow stdout`, `allow stderr`                                    |
| `RulePolicy::allow_all()` | a rule that allows each kind of capability (trusted scripts only) |

The most specific matching rule decides a request. Of two rules with equal
specificity, `deny` wins over `allow`, and `allow` wins over `prompt`, so a
rule saved from a prompt answer decides that request. A request that no rule
matches gets `Prompt` (§14.3): the prompter decides it, or the engine denies
it. The `Display` of a `RulePolicy` lists its rules as policy file text
(§14.7), and `RulePolicy::extend` adds the rules of another policy.

`rules.with_defaults(defaults)` adds the rules of `defaults` as default rules,
which decide only the requests that no other rule matches. A host combines
the user's rules with its own this way, as the CLI does (§17.5). The default
rules of `defaults` stay below its other rules. Resolved addresses (§14.6) are
the exception: there, rules compete by host specificity alone, so a user's
domain pattern does not vouch for an address that a default rule denies.

Specificity depends on the kind of request:

- **Files** (`FsRead`, `FsWrite`, `Import`)
  - a path pattern matches the path and its subpaths: `/data` covers `/data/a/b`
  - Each path segment may be a string, a name with `*` wildcards (`*.pem`),
    standalone `*`, or standalone `**`. A pattern that starts with `**`
    matches on every root, including all drives on Windows: `**` alone
    matches every path.
  - A path is "more specific" if it has more matching segments. For example,
    `deny ~/**/*.pem` is more specific than `allow ~/project` for `~/project/key.pem`
  - If two paths have equal depth, the one with fewer wildcards is more specific
  - Write access implies read access
  - Read denial implies write and import denial

- **Network Requests** (`Net`)
  - a host pattern may match any domain `*`, a specific domain `example.com` or strict subdomains `*.example.com`
  - a host pattern may also match an IP address (`127.0.0.1`, `::1`) or a CIDR range (`10.0.0.0/8`, `fc00::/7`),
    optionally with a port (`host:443`, `[::1]:8080`, `10.0.0.0/8:443`, `[fc00::/7]:443`).
    The host bits after the prefix length of a CIDR range must be zero.
  - A rule matches connections over the transports it names: `tls`
    (`net.connect_tls`, `https`), `plaintext` (`net.connect`, `http`), or
    both. A rule that names no transport matches `tls` if it allows, and both if it denies or
    prompts, so `deny net 10.0.0.0/8` also stops `http://10.0.0.5/`.
  - Exact hosts and addresses are most specific. Then come patterns of names under `localhost`
    (`*.localhost`, since RFC 6761 makes every such name the loopback host), then ranges and domains
    (longer ones more), then `*`. An IPv4 range ranks as the IPv6 range that embeds it
    (`10.0.0.0/8` as a `/104`).
  - A port increases rule specificity. Transports do not: of an allow and a deny rule for the
    same host and transport, the deny rule wins.
  - Host names must be ASCII and are compared case-insensitively, ignoring a trailing dot
  - IPv4-mapped IPv6 addresses are compared as IPv4.
  - Deny and prompt address patterns also match other forms of their addresses: `0.0.0.0` and `::`
    are the same address, and they also connect to `127.0.0.1` and `::1`. An IPv6 address that
    embeds an IPv4 address (NAT64, 6to4, IPv4-translated, IPv4-compatible) is also that IPv4
    address. Thus `deny net 127.0.0.1` also denies `0.0.0.0`, `::` and `::ffff:0:7f00:1`. Allow
    patterns match only the address that the script used.
  - In the same way, the deny and prompt host `localhost` also matches every name under it
    (`app.localhost`), which is the same loopback host. `allow net localhost` matches only
    `localhost`. As for every name, the rule does not match IP addresses: `deny net localhost
    127.0.0.0/8 ::1` keeps scripts off the loopback host.

- **HTTP(S) requests** (`Http`)
  - inherits net rules for the host pattern, port and transport
  - adds http methods and paths
  - Specificity compares the host (as for `Net`), then path, then methods.
  - URL paths are compared percent-decoded, without `.` and `..` segments
  - A request whose path contains `\` or an encoded separator (`%2F`, `%5C`) gets the
    stricter decision of its two readings, with and without those taken as separators.
  - A net rule allowing a host lets scripts send it anything over `net`, even if `deny http` rules say otherwise

- **Environment variables** (`Env`)
  - supports a name `AWS_SECRET_KEY` or a pattern (`AWS_*`), where `*` matches any characters
    (`**` is an error)
  - specific names are prioritized above wildcard patterns
  - `Env("*")` (all variables) gets the strictest decision that any one variable gets: `Deny`,
    then `Prompt`, then `Allow`. A variable that no rule matches gets `Prompt`, so only a rule
    such as `allow env *` allows the listing

- **Listen addresses** (`NetListen`)
  - patterns are `ip:port`, `[ipv6]:port`, `ip`, `localhost` (any loopback address), `*:port`, or `*`
  - host `*` matches any address, port `*` matches any port, and port `0` matches only an ephemeral port request
  - IPv4-mapped IPv6 addresses are compared as IPv4
  - specificity compares the host (`ip` or `localhost` before `*`), then the port

- **Custom capabilities**:
  - a native module may define its own capabilities
  - a rule names one (`custom db`) or all (`custom *`), with an optional detail
    (`custom db drop`; `*` or none: any detail); a name is more specific than
    `*`, and a detail more specific than none. Names and details are exact:
    quote a `*` that is part of one (`custom db "a*b"`)

Path rules are compared after normalization (§14.1). A rule's path is
resolved when the policy decides its first request. Allow rules compare
names exactly, ignoring case on Windows. Deny and prompt rules, on every platform,
compare case-insensitively and normalization-insensitively (canonical
decomposition, full case folding, default-ignorable characters removed), so
they also cover other spellings, including of paths that do not exist yet
(`data/ſecrets`, `café` precomposed or with a combining accent)

On case-insensitive file systems, the engine resolves existing paths to
on-disk names (`data/SECRETS` becomes `data/secrets`). For directory entry operations (§13.3),
`RulePolicy::check_entry` evaluates the on-disk name without resolving symlinks in the last path component.
If the system cannot read an on-disk name (a symlink outside macOS, a device, or an unopenable file),
the engine scans the parent directory if it contains at most 4096 entries.

A directory that holds an entry decided by a deny or prompt rule cannot be renamed, replaced
or recursively removed as a whole. The tree is scanned for such entries only if
`RulePolicy::denies_under` reports that a deny or prompt rule may match a path in it. For
custom policies (`CustomPolicy`), the engine invokes `denies_under` and `protects`
on the tree. By default, the engine scans trees and protects entries when reading
or writing the entry is denied.

### 14.6 Network destinations

The engine checks an outgoing connection (`net.connect`, `net.connect_tls`,
`http` requests and redirect hops) in two stages:

1. `Net(host, port, tls)` (for `http`, `Http(method, url)`) for the host name
   or IP literal that the script used. The policy can prompt for this request.
2. The engine resolves the name once and checks each address as
   `NetAddr(host, addr, tls)`. An IP literal resolves to itself. `localhost` and
   the names under it resolve to `127.0.0.1` and `::1` without the system
   resolver (RFC 6761). The engine connects only to allowed addresses, in
   resolver order. If the policy denies all addresses, the operation fails
   with kind `"permission_denied"`, the address and the deciding rule.
   `NetAddr` never prompts.

A rule policy decides an address in stage 2 with `RulePolicy::check_resolved`.
The rule that allowed the stage 1 request (or prompted for it) competes with
the address rules by the specificity of their hosts. The address rules are
the rules for IP addresses that match the address and the request's transport:
`net` rules, and for an HTTP request also `http` rules that match its method
and path. An exact host name ranks as a single address, and
a pattern of names under `localhost` ranks above every range. Other domain
patterns and `*` rank below every address rule, because they name hosts that
the user does not know. If the stage 1 rule wins, or no address rule matches,
the engine connects. Otherwise the address rule decides, and a `prompt`
address rule denies, because the engine cannot ask after resolution. A custom
policy decides `NetAddr` itself, and `Prompt` counts as `Allow`.

So a deny rule for a range also stops the names that resolve into it. An
attacker can point a public name at a local service: with `allow net
plaintext *` and `deny net 10.0.0.0/8`, a request to `http://evil.example/`
that resolves to `10.0.0.5` fails. A rule that allows
an exact name, such as `allow net db.internal`, outranks the range and lets
that name reach `10.0.0.5`. `sandlang::policy::is_private_address` tells
whether an address is in a private, loopback, link-local, or other
non-public range.

### 14.7 Policy files

A policy file (`.policy`) contains policy rules and limits as text. Read it with
`sand run --policy=FILE` (§17.5); parse it with `sandlang::policy::PolicyFile::parse(text, base, home)`,
which gives its rules (`PolicyFile::rules`) and limits (`PolicyFile::limits`). The
rules are the whole policy: a host that wants scripts to print extends
`RulePolicy::standard()` with them.
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
prompt write ./dist                      # ask before writing the build output
deny ~/Documents/*
allow http GET HEAD api.github.com
allow http POST PATCH api.github.com/repos/{owners}/**
allow net plaintext localhost:8080       # the development server, over http://
allow env HOME USER LC_*
limit memory 64M                         # the prompter decides the rest
```

| Statement                                                 | Meaning                                                                         |
| --------------------------------------------------------- | ------------------------------------------------------------------------------- |
| `allow PATH...`, `deny PATH...`                           | all access to files (§14.5): `allow` reads and writes, `deny` denies everything |
| `allow read PATH...`, `deny read PATH...`                 | `FsRead` (a deny also denies writing and importing)                             |
| `allow write PATH...`, `deny write PATH...`               | `FsWrite` (an allow also allows reading)                                        |
| `allow import PATH...`, `deny import PATH...`             | script imports                                                                  |
| `allow net [TRANSPORT...] HOST...`, `deny net ...`        | connections, and any HTTP request, to hosts (§14.5)                             |
| `allow http [TRANSPORT...] [METHOD...] URL...`, ...       | HTTP requests                                                                   |
| `allow listen ADDR...`, `deny listen ADDR...`             | listening addresses (§14.5)                                                     |
| `allow env NAME...`, `deny env NAME...`                   | environment variables (`AWS_*`; `*`: all)                                       |
| `allow stdin`, `deny stdin`, `deny stdout`, ...           | standard streams                                                                |
| `allow timezone`, `deny timezone`                         | the local time zone                                                             |
| `allow custom NAME [DETAIL]`, `deny custom NAME [DETAIL]` | a host-defined capability (`*`: any name or detail; any detail if omitted)      |
| `prompt ...`                                              | as `allow ...`, but asks the prompter (§14.5)                                   |
| `limit NAME VALUE`                                        | a limit (§15.1), below                                                          |
| `let NAME = VALUE...`                                     | a set: `{NAME}` in any rule's word stands for each value                        |

All paths must be anchored, but they may be prefixed with a shortcut:

- `/...` begins at the root directory
- `./...` begins at the file's directory
- `../...` begins at the file's parent directory
- `~/...` begins at the user's home directory
- `**/...` begins at any directory, on every root

Transports (`tls`, `plaintext`, or both, in any case) precede the hosts of
`net` and `http` rules. Without one, an allow rule matches TLS only, and a
deny or prompt rule matches both. In their place, the scheme and protocol
names `http`, `https`, `ws`, `wss`, `tcp`, `udp` and `ssl` are errors. A
quoted word is never a transport or a method, and a trailing dot names a host
with such a name (`tls.`, `delete.`).

URL patterns have the form `host[:port][/path]`, where `host` is a hostname,
domain wildcard, or single IP address and `path` is a percent-decoded file
path pattern. A pattern without a port matches all ports. URL patterns take no
scheme: `https` is `tls` and `http` is `plaintext`. An `http` rule for an IP
address also decides the requests to names that resolve to it (§14.6).

HTTP Methods (`GET`, any case, or any upper-case word) precede URLs, before or
after the transports. If no method is present, an http rule matches every method.

A word with multiple set references allows every combination of set values
(`{owners}/{repos}`), limited to 100,000 patterns per word (and per custom rule). A set may be defined
anywhere in the file. A set may not contain another set.

A request that no rule matches gets `Prompt`: the host's prompter decides
it, or the engine denies it.

A policy file may contain limit rules:

- `memory`, `output`, `source` and `http-body` take sizes (`4096`, `64K`, `256M`, `1G`)
- `time` (wall clock) and `cpu` durations (`500ms`, `10s`, `1.5m`, `2h`, `1d`)
- `instructions`, `compile-fuel`, `tasks`, `call-depth` and `open-resources` counts (`1_000_000`)
- `none` removes `memory`, `time`, `cpu`, `instructions`, `compile-fuel` and `output` limits.

Each system resource category that is not explicitly specified has a default value.

A policy file with any error is rejected. Each `PolicyError` provides the line and
column containing the error.

Policy files are covered by `sand fmt`, `sand lint` and `sand check` (§17).
The formatter separates words by single spaces, adds quotes where necessary
and keeps the quotes of quoted text, writes transports in lower case and
methods in upper case, aligns the trailing comments of consecutive
lines and removes runs of blank lines. `sandlang::policy::lint_policy`
reports:

| Lint                   | Description                                                                                                          |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------- |
| `redundant-rule`       | broader rules or an earlier identical rule already make the same decision                                            |
| `overridden-rule`      | higher-precedence rules make another decision for all matching requests                                              |
| `bypassable-http-deny` | a `net` rule permits raw TCP connections to a destination that a `deny http` or `prompt http` rule restricts         |
| `unused-set`           | a `let` set is not used in any rule                                                                                  |

An allow rule for an exact host name is not redundant under a domain pattern
if an address rule ranks between them, because it also decides the addresses
that the name resolves to (§14.6). File paths are compared lexically without
resolving symlinks.

`sandlang::policy::rule_for(capability, allow)` gives, as a policy file
line, the rule that decides a promptable request for good (e.g. `allow read
/data/a.txt`, `deny http GET example.com:443/a`), quoted as needed; the
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

In a session (§16.6, §17.6), the instruction, CPU time, wall time and
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
by an order of magnitude or more is charged by time. All other fuel charges
are deterministic across runs; time-based regex charges depend on execution
speed.

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

The `sandlang` crate embeds the Sand compiler and virtual machine in a Rust host
application. Each `Engine` encapsulates an isolated execution environment
configured through `Config` with a sandbox `Policy` (§14), resource `Limits`
(§15), host I/O providers, and optional `NativeModule` definitions. Engines do
not share mutable state and `Engine` implements `Send`, so a host can run
multiple engines concurrently across threads or asynchronous tasks (or
interleaved on a single thread in WebAssembly, §16.5).

```rust
use sandlang::{Config, Engine, Limits, NativeModule, Policy};

let mut module = NativeModule::new("geo");
module.function("dist", |a: f64, b: f64| -> f64 { (a * a + b * b).sqrt() });
module.async_function("lookup", |key: String| async move { Ok::<_, String>(key.len() as i64) }).params(&["key"]);

let config = Config::new()
    .policy(RulePolicy::standard())
    .limits(Limits { max_memory: Some(64 << 20), ..Limits::default() })
    .module(module);
let mut engine = Engine::new(config)?;
let outcome = engine.run_source("main.sand", r#"import geo; print(geo.dist(3.0, 4.0))"#).await?;
```

### 16.1 Execution and errors

`Engine::new` validates registered host modules (§16.3) and constructs an
`Engine`. `Engine::run_source` and `Engine::run_file` compile and execute a
script inside
the returned future on the thread that polls it. `Engine::run_tests` compiles a
module and executes its `test` blocks (§4.8). For static analysis without
execution, `check_source`, `check_file`, `lint_source`, and `lint_file` (§17.2)
resolve imported modules from the engine's file system under its policy, except
that any import requiring an interactive permission prompt is denied
immediately.

A completed run returns `Outcome { exit_code: i32 }`, and a failed operation
returns an `Error`:

| Variant                              | Meaning                                                              |
| ------------------------------------ | -------------------------------------------------------------------- |
| `Error::Compile(diagnostics)`        | Syntax or static type errors occurred during compilation (§18).      |
| `Error::Runtime { message, trace }`  | Execution failed with an uncaught runtime error (§10.3).             |
| `Error::Limit(kind)`                 | Execution or compilation exceeded a configured resource limit (§15). |
| `Error::Interrupted`                 | The host cancelled execution through an `InterruptHandle`.           |
| `Error::Io(message)`                 | The entry script file could not be read.                             |
| `Error::Internal { message, trace }` | An internal invariant failed or a host callback panicked (§10.3).    |

If `sandlang` is compiled with `panic = "unwind"` (the default), the engine
catches Rust panics inside library native functions, host callbacks, I/O
operations, and permission prompters and converts them into `Error::Internal`
so that panics do not unwind into the host.

### 16.2 Host environment and providers

`Config` allows the host to virtualize every external input, output, and source
of nondeterminism—streams, logging, process metadata, randomness, time, file
systems (§16.4), and networking—so that script execution can be made completely
deterministic. By default, `Config::new()` has the policy `RulePolicy::standard()`, which
allows only `Stdout` and `Stderr`, and no prompter, so the engine denies every
other request (imports too, unless a rule such as `allow import ./lib` allows
them). It writes output to the process's standard streams and provides an
empty standard input.

The host can capture standard output and standard error or feed standard input
into a running script. `Config::stdout` and `Config::stderr` redirect script
output to an `OutputSink`, while `Config::stdin_bytes`, `Config::inherit_stdin`
(feature `os`), and `Config::stdin_feed` configure standard input. With
`StdinFeed`, scripts read chunks pushed via `StdinFeed::send` (`StdinFeed::end`
terminates a single read), and `StdinFeed::wanted` is resolved if a script is
waiting for input and all other tasks are blocked.

The host can intercept and filter structured log records (§13.8) instead of
writing them to standard error. `Config::log_sink` routes records to a
`LogSink` as `LogRecord { level, message, fields, location }` on the polling
thread, using `LogSink::enabled` to filter records by level before allocation.
`Config::log_level` sets the initial minimum script log level (default
`LogLevel::Info`), or disables script logging if set to `None`.

Command-line arguments, environment variables, and random number generation can
be fixed per engine. `Config::args` and `Config::env` replace the process
arguments and environment variables exposed by `env` (§13.6), and
`Config::seed` seeds the pseudorandom number generator used by `random`
(§13.11) and `uuid` (§13.18).

Custom clock and time zone providers decouple script execution from the system
clock. `Config::clock` supplies a `platform::Clock` that drives wall time,
monotonic time, `MemFs` timestamps, UUIDv7 timestamps, sleep timers, execution
deadlines, and network timeouts, while receiving fuel consumption reports via
`Clock::work` and scheduler yields via `Clock::yield_now`. `Config::time_zone`
sets the `time::TimeZone` returned by `time.local_zone()` if the policy allows
`Capability::TimeZone`.

A custom `network::Network` provider can intercept all DNS resolution, outgoing
TCP streams, and TCP listeners in `net` and `http`. If `Config::network` is
set, the engine still checks the network policy before connecting or listening
and negotiates TLS over the returned stream; `Network::listen` must bind only
the requested address and must not accept IPv4 connections on an IPv6 listener.
`Config::tls_root_certificates` adds DER-encoded root certificates to the
bundled Web PKI roots (an invalid certificate causes TLS handshakes to fail
with error kind `"io"`).

Module search paths and capability policies are configured on `Config` before
constructing an `Engine`. `Config::module_root`, `Config::cwd`, and
`Config::library` set the directories used for relative file paths and `import`
resolution (§3.2), where registered standard and host modules
(`Engine::module_names`) take precedence over library scripts. `Config::policy`
and `Config::prompter` set the active `Policy` and interactive `Prompter`
(§14).

### 16.3 Native modules

`NativeModule` exposes Rust functions and types to Sand scripts as an
importable module registered with `Config::module`. The engine infers the Sand
signature of each registered function from its Rust parameter and return types,
and charges fuel and memory when converting arguments and return values.
`NativeModule` and `Config::module` panic if a module, type, or function name
is invalid or already registered, or if a host module uses the name of a
standard library module.

| Method                       | Registered item                                                                     |
| ---------------------------- | ----------------------------------------------------------------------------------- |
| `function(name, f)`          | synchronous function that can borrow `&str`, `&[u8]`, and `&T` arguments            |
| `vm_function(name, f)`       | synchronous function that takes `&mut Vm` and owned arguments                       |
| `async_function(name, f)`    | asynchronous function that takes owned arguments and suspends only the calling task |
| `bind::<T>()`                | Rust type `T` with a derived `NamedType` (see below)                                |
| `params(names)`, `doc(text)` | parameter names and documentation for the most recently registered function         |

Four derive macros bind a Rust type to a Sand type in a native module and
implement `NamedType`, `SandType`, `FromValue`, and `IntoValue`. Every derived
type requires `#[sand(module = "<module>")]` and accepts `#[sand(name = "<Name>")]`
to override its Sand name, `#[sand(private)]` to keep the type private to its
module, and `#[sand(crate = <path>)]` to specify a renamed `sandlang`
dependency. Rust doc comments (including `#[doc = include_str!("...")]`) become
Sand documentation. Compilation fails if a type, variant, or field name is a
Sand keyword or is not a valid Sand identifier.

| Derive       | Sand type                                                                  |
| ------------ | -------------------------------------------------------------------------- |
| `SandValue`  | opaque value type (§5.4); the type must also implement `ValueType`         |
| `SandEnum`   | `enum` with the variants and payloads of a Rust enum                       |
| `SandObject` | opaque reference type; the type must also implement `NativeObject`         |
| `SandStruct` | `struct` with the named fields of a Rust struct, copied on each conversion |

Each field of a `SandStruct` and each payload of a `SandEnum` variant must
implement `IntoValue` and owned `FromValue`. Unit variants map to unit
variants, tuple variants map to variants with positional payloads
(`Circle(float)`), and struct variants map to variants with named payloads
(`Rect(w: float, h: float)`), where `#[sand(name = "<name>")]` overrides a
payload field name. Generic type parameters on a `SandEnum` or `SandStruct` map
to Sand type parameters; pass any concrete instantiation to `bind` (such as
`bind::<Pair<(), ()>>()`).

By default, struct fields are `pub` and immutable; `#[sand(private)]` makes a
field private to the module, `#[sand(var)]` makes it mutable (`var`), and
`#[sand(name = "<name>")]` overrides its Sand name. `#[sand(default)]` uses
`Default::default()` as the Sand field default, and `#[sand(default = <expr>)]`
uses `<expr>`. `bind` panics if a field default is not a constant (built from
numbers, `bool`, `()`, strings, `bytes`, durations, tuples, `Vec`, maps, sets,
`Option`, `SandEnum`, and `SandStruct` values, with at most 65,536 nodes and 64
nesting levels).

Recursive types can reference themselves through `Vec`, map, or `Box` fields.
Because the engine copies each reachable struct and enum value during
conversion and charges fuel and memory for each copy, shared references in a
Sand value produce independent Rust copies. Conversion aborts with kind
`"limit"` if the nesting of structs and enum payloads exceeds 256 levels (for
example, in a cyclic value). Module Sand source can define methods on any bound
type with `extend`.

The Rust return type of a registered function determines whether the function
returns a Sand `Result` or aborts the run on failure:

- `Result<T, HostError>` maps to the Sand return type `Result<T, Error>`.
  Returning `Err(HostError::new(kind, message))` yields `Err(Error(message,
kind: kind))` in Sand, whereas `?` converts a `Ctl` into a `HostError` that
  halts or suspends the VM and converts a `std::io::Error` into a `HostError`
  with the corresponding standard error kind.
- `Result<T, std::io::Error>`, `Result<T, String>`, and `Result<T, &str>` also
  map to `Result<T, Error>`; string errors use the kind `"error"`.
- `Result<T, Ctl>` maps to the Sand return type `T`. Returning `Err(ctl)`
  aborts the run with a runtime error (`Ctl::fail`), a resource limit error, or
  an exit code.
- `Result<Later<T, E>, E>` suspends the calling task on a future and maps to
  the same Sand return type as `Result<T, E>`.

To check a sandbox capability inside a `vm_function`, call
`Vm::require(&capability)` (§14), which returns an error of kind
`"permission_denied"` if the policy denies the request. Because the engine
re-runs a `vm_function` from the start after a permission prompt or after a
garbage collection triggered by a memory or resource check (`Vm::check_memory`,
`Vm::reserve_io`, `Vm::resource_slot`), the function must check capabilities and
limits before performing any side effects. `Config::retry_stress` tests this
requirement by failing the first memory or resource check in each `vm_function`
call. Return-value conversion does not fail on the memory limit; instead, the
engine collects garbage and checks the memory limit after the call completes.

| Rust type                                          | Sand type                                              |
| -------------------------------------------------- | ------------------------------------------------------ |
| `i64`, `f64`, `bool`, `()`, `jiff::SignedDuration` | `int`, `float`, `bool`, `()`, `duration`               |
| `String`, `&str`; `Vec<u8>`, `&[u8]`               | `str`; `bytes`                                         |
| `Vec<T>`, `Option<T>`, tuples of 2 to 8 elements   | `[T]`, `T?`, tuples                                    |
| `Box<T>`                                           | `T`                                                    |
| `HashMap<K, V>`, `BTreeMap<K, V>`, `IndexMap<K, V>` | `[K: V]`                                              |
| `HashSet<T>`, `BTreeSet<T>`                        | `Set<T>`                                               |
| `Result<T, E>`                                     | `Result<T, Error>` or `T` (determined by `E`)          |
| `Never`                                            | `never`                                                |
| `sandlang::json::Json`                             | `json.Json`                                            |
| bound type `T` (see above)                         | `T` (or `&T` for a borrowed object parameter)          |
| `Val<T>`                                           | `T` (passed as an uncopied heap handle)                |
| `Param<N, B>`, `Reflect<N>`                        | `N`-th type parameter and its runtime type description |
| `Num<N>`                                           | `Num` evidence of the `N`-th type parameter            |

`Param<N, B>` specifies the built-in traits `B` (`traits::Eq`, `traits::Ord`,
`traits::Hash`, `traits::Num`, `traits::Encode`, or a tuple such as
`(traits::Ord, traits::Hash)`, defaulting to `()`) required on the `N`-th type
parameter. The `native fn` declaration must declare these traits for that
type parameter, directly or through traits that have them as supertraits.
Native functions can only require built-in traits.

If a `native fn` declaration requires `Num` of its `N`-th type parameter, the
Rust function can accept a `Num<N>` argument (providing `Num::zero` and
`Num::one`) after any `Reflect` arguments and in type-parameter order. Native
`impl` methods cannot take `Num<N>`.

The engine automatically generates Sand declarations for registered types and
functions that are not declared in `NativeModule::source`. Registered types use
their Rust doc comments, and undeclared functions are exported as `pub native
fn` with parameter names `arg1`, `arg2`, ... unless overridden by `params`.
`help` lists registered types and functions in registration order before items
declared in `NativeModule::source`. A function registered as `"Type.name"` is a
method on `Type`, and takes `self` if its first parameter has type `Type`.
A function registered as `"Type.init"` constructs `Type` (§4.2).
Generic functions and functions with default parameter values must be declared
explicitly with `native fn` in `NativeModule::source`.

`Engine::new` compiles and validates all registered host modules once per
`Config` (shared across cloned configurations), and returns `Error::Compile` if:

- the module's Sand source contains a syntax or type error;
- a `native fn` declaration in Sand source does not match the Rust function's
  signature;
- a registered function references an unregistered type or another module's
  private type;
- a public function or field exposes a private type;
- a `native fn` declaration does not declare a trait that its Rust function
  requires;
- a generic Rust function lacks a `native fn` declaration in Sand source;
- a method registered as `"Type.name"` targets a nonexistent type, or targets a
  generic type without a `native fn` declaration in Sand source;
- host modules form a dependency cycle through their registered function types;
- a function declared in Sand source also configures `params` or `doc` in Rust.

### 16.4 Operating-system access and file systems

Two Cargo features control whether an engine can access the host operating
system:

- `os` (enabled by default): enables access to the host file system, network
  sockets (`net` and `http`), process standard input (`Config::inherit_stdin`),
  and process environment variables (if `Config::env` is not set). If `sandlang`
  is built without `os`, the engine cannot access the operating system
  regardless of the configured policy: `net` and `http` are unavailable, the
  environment is empty unless set via `Config::env`, standard input comes only
  from `Config::stdin_bytes` or `Config::stdin_feed`, scripts use an in-memory
  file system, and the crate compiles for `wasm32-unknown-unknown` (§16.5).
- `tls` (enabled by default; implies `os`): enables TLS support for `https` and
  `net.connect_tls` using `rustls` and `ring` (which requires a C compiler for
  the target). Build with `default-features = false, features = ["os"]` to
  disable TLS while keeping OS access.

`Config::memory_fs` replaces host disk access with an in-memory `MemFs` for the
`fs` module, script imports, and `Engine::run_file`. Without the `os` feature,
each engine uses an empty `MemFs` unless `Config::memory_fs` provides a
pre-populated instance. Clones of a `MemFs` share the same underlying files, so
the host can populate input files before a run, inspect output files afterward,
or share a virtual file system across engines (`write`, `read`,
`create_dir_all`, `remove`, `clear`, and `files`). `MemFs::with_capacity`
bounds total storage (default 64 MiB, counting 64 bytes of overhead per entry
in addition to its name and contents). Working directory and module root paths
are resolved inside the `MemFs` (default `/`), and `RulePolicy` evaluates `MemFs`
paths against lexically normalized policy rules on all platforms.

```rust
let fs = MemFs::new();
fs.write("/data/in.txt", "input")?;
let config = Config::new().memory_fs(fs.clone()).policy(
    RulePolicy::standard().allow_read("/data").allow_write("/out"));
// ... run a script; then:
let report = fs.read("/out/report.txt")?;
```

### 16.5 WebAssembly

If built without the `os` feature, `sandlang` compiles to
`wasm32-unknown-unknown` for execution in a JavaScript host via `wasm-bindgen`.
JavaScript supplies the clock and timers, and the compiler's recursive passes
run on the WebAssembly linear memory stack (sized for the maximum parser
nesting depth). The engine executes code synchronously on the calling thread and
yields to the JavaScript event loop only while waiting for a timer, an
asynchronous I/O operation, or a permission prompt; hosts should run engines
inside Web Workers to avoid blocking the UI thread and to run multiple engines
in parallel.

The `sandlang-wasm` crate (`wasm/`) exposes this build as a JavaScript `Engine`
class configured with a policy file, initial virtual files, environment
variables, arguments, standard input, a time zone, and `onOutput` and
`onPrompt` callbacks. A prompt without an `onPrompt` callback is denied.
The library provides `run`, `check`, `lint`, `format`, and `highlight` methods
for typical execution and static analysis tasks.

### 16.6 Sessions

`Engine::session()` creates a stateful `Session` (`Send`, with `Send` futures)
that evaluates inputs incrementally under the engine's configuration and shares
the engine's remembered `"always"` prompt decisions (§14.3). `Session`
implements the evaluation semantics of `sand repl` (§17.6):

```rust
let mut session = Engine::new(config)?.session();
session.eval("var hits = 0\nfn hit() -> int { hits += 1\n hits }").await?;   // Ok(None)
assert_eq!(session.eval("hit() + hit()").await?, Some("3".to_string()));
```

`Session::eval(source)` or `eval_as(name, source)` type-checks, compiles, and
executes a single input, returning `Ok(Some(value))` with the debug representation
of a trailing expression or `Ok(None)` for statements and declarations. If an
input calls `exit(code)`, `eval` returns `Ok(None)`, `exit_code()` returns `Some(code)`
until the next input, and the session remains usable. If an input imports a
script module whose file or transitive script dependencies changed since the
previous load, the session reloads that module while bindings from earlier
inputs keep their previously loaded versions. `type_of(expr)` returns the
static type of an expression without executing it or mutating session state,
`run_tests()` executes all `test` blocks defined in the session, and
`Session::is_complete(source)` reports whether a multi-line input is
syntactically complete (§17.6).

`Session::run_tasks_until(until, on_error)` polls tasks spawned by earlier
inputs until the future `until` completes, returning the output of `until` (or
`None` immediately if a task calls `exit`). Instruction, CPU-time, and output
limits apply to each call, whereas `max_wall_time` does not, and permission
prompts wait until `until` completes. If a background task fails with an error,
limit, or interruption, the session cancels all running tasks, passes the error
to `on_error`, and continues waiting for `until`. `cancel_tasks()` cancels
remaining background tasks without running their `defer` blocks.

`Session::set_wall_time` overrides `max_wall_time` for subsequent inputs, and
`session.interrupt_handle()` cancels the active session call (the parent
engine's interrupt handle does not affect sessions). Dropping an `eval` future
before completion cancels the input in the same way as an interrupt, and leaves
the session state unchanged if the input was still loading imports. `reset()`
clears all session bindings and state. If an internal error (§10.3) occurs
during `eval`, `run_tests`, `type_of`, or `run_tasks_until`, the session
automatically resets itself, appends `" (the session was reset)"`
(`sandlang::session::RESET_NOTE`) to the error message, and sets `was_reset()`
to `true` until the next call while preserving remembered `"always"` prompt
decisions.

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
| `argument-order`       | positional construction argument or sub-pattern whose identifier matches a different field or `init` parameter (`Rect(h, w)` for `struct Rect { w: float, h: float }`)                                                                  |
| `len-zero`             | `x.len() == 0` (use `is_empty()`)                                                                                                                                                                                                       |
| `nan-comparison`       | a comparison with `nan`, which is always false (`!=` is always true); use `is_nan()`                                                                                                                                                    |
| `empty-block`          | empty `if`/`else`/loop body                                                                                                                                                                                                             |
| `todo`                 | call to `todo()`                                                                                                                                                                                                                        |
| `shadowed-variable`    | `let`/`var` binding a name that is already bound, with a different type, by a binding of the same function that is still in scope (see below)                                                                                           |
| `variant-like-binding` | a name starting with an upper-case letter that binds a new variable in a pattern (`match`, `if let`, `while let`, `let`, `for`), usually a mistyped variant without a payload (see below)                                               |
| `unclosed-resource`    | a resource (§7.2) created and never closed: bound with `let`/`var` or used as a temporary, and neither closed by `use` nor handed on (see below)                                                                                        |
| `resource-field`       | a struct field or enum variant holding a resource, in a type that does not implement `Close`                                                                                                                                            |
| `resource-escapes`     | a resource bound with `use`, or received as a parameter, that is kept beyond the call: returned (`use` only), stored, captured by a task, or passed to a function that keeps it                                                         |

`unused-function`: a module's public entry points are its `pub` top-level
functions and the `pub` methods of its `pub` types. The linter reports any
private function or method that is not reachable from top-level code, a `test`
block, or a public entry point through calls, method calls, function values,
default arguments, or `help` and `members` arguments (including functions
referenced only by themselves or by other unused functions).

`shadowed-variable`: the earlier binding may be a parameter, a `let`/`var`
(including destructuring), a loop variable, or a `match`/`if let`/`while
let`/lambda/comprehension binding, and must still be in scope, so bindings
in sibling blocks (two `if` branches, two loops, two `match` arms) do not
shadow each other. A lambda belongs to its enclosing function, but each
named function and `test` block is checked on its own, so re-binding a
top-level name is not reported.

The resource lints follow resources through each function. A resource is
_created_ by a call that returns one (or a `Result`, option or tuple holding
one) unless its result may come from an argument, and by a construction or
literal of a `Close` type (struct, list, option, ...), which takes over the
resources it is built from. It is _handed on_ if it is returned, stored (in a
field, list, map, set, global or channel), captured by `spawn`, taken over by a new
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
types, `extend`, traits, impls and tests, line width 100 (the formatter breaks
calls, lists, maps, and parameter lists that exceed the line width across
lines with each element on its own line, breaks patterns across lines if doing
so allows the rest of the line to fit, and breaks method chains that exceed the
line width before each method call), and comments preserved (the formatter
converts any `///` comment that does not document an item to `//`).
Trait and impl bodies are laid out like struct bodies,
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
policy and limit options of `sand run` (§17.5) and, like it, asks about
what its rules do not decide. A test that exceeds a limit fails with the
limit's message.

### 17.5 `sand run FILE [-- ARGS...]`

The CLI has these default rules: `allow stdout`, `allow stderr`, `allow
stdin`, `allow timezone` and `allow import **`. A default rule decides only
the requests that no rule of the user (an option or a policy file rule)
matches (`RulePolicy::with_defaults`), so `--prompt=stdin` asks before the
script reads standard input. The prompter decides every request that no rule
decides: the CLI asks the user on the terminal.

Options: `--allow=RULE`, `--deny=RULE` and `--prompt=RULE` add the rules of a
policy file line (§14.7) without its verb, such as `--allow='read ./data'`,
`--allow='net plaintext localhost:8080'` or `--deny=stdin`; relative paths
start from the working directory. `--policy=FILE` adds the rules of a policy
file and its limits (the options add to its rules and override its limits).
The options and the policy file's rules are one set of rules: of two rules
with the same pattern, `deny` wins (§14.5), so an option cannot loosen a
`deny` rule of the file. `--allow-all` makes `RulePolicy::allow_all()` the
default rules, so it allows what no rule of the user matches:
`--allow-all --prompt='write **'` asks before every write. `--no-prompt`
denies what the CLI would ask about. An option that takes a value takes it
after `=` or as the next argument (`--allow 'read ./data'`). Limits: `--max-memory=BYTES` (suffixes
K, M, G), `--max-time=DURATION`, `--max-cpu=DURATION`,
`--max-instructions=N`, `--max-compile-fuel=N`. `--seed=N` seeds the random
number generator. Arguments after `--`, and positional arguments after
`FILE`, are passed to the script. Exit code: the script's exit code (codes
outside `0..=255` exit with 255), 1 for a runtime error or `?` failure, 2 for
compile errors, 3 for limit violations. If the policy allows `TimeZone`,
`time.local_zone()` returns the system time zone.

The CLI asks on the controlling terminal (`/dev/tty`; the console on
Windows), never on the script's standard streams, so piped input cannot
answer. Without a terminal, or with `--no-prompt`, the CLI denies the request
for the rest of the run and writes the option that allows it to standard
error (`sand: denied: read /data/a.txt (--allow='read /data/a.txt' allows
it)`). Answers:
`y` (allow once), `a` (always), `n` or an empty line (deny once), `N`
(never); end of input denies, and an unrecognized answer is asked again up to
three times before denying. Control characters in the request are escaped,
as are Unicode format characters (such as bidirectional overrides), line and
paragraph separators, spaces other than U+0020 and combining marks. Before
each question the terminal's rendition is reset (soft terminal reset and SGR
reset, on a fresh line) and a marker line is shown, so output the script left
concealed or styled cannot hide or imitate the question.

### 17.6 `sand repl [OPTIONS]`

Interactive read-eval-print loop, taking the options of `sand run` (§17.5).
It runs a session (§16.6): each input is type checked in the context of the
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
  earlier inputs with the same name or for the same type. A later input cannot
  add `init` (§4.2) to a type declared in an earlier input.
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
read standard input through the REPL: while an input runs,
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
