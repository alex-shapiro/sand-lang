# The Sand Language Specification

Version 0.1

Sand is a small, statically typed, expression-oriented scripting language with a
sandboxed bytecode virtual machine written in safe Rust. It is designed to be
embedded: every effect a script can have on the outside world goes through a
host-defined policy, and every engine runs under host-defined resource limits.

This document is normative. Sections marked _(informative)_ are not.

---

## Contents

1. Design goals _(informative)_
2. Source text and lexical structure
3. Programs, modules and imports
4. Declarations
5. Types
6. Expressions
7. Statements
8. Patterns
9. Type checking and inference
10. Runtime semantics
11. Concurrency and asynchronous I/O
12. The prelude
13. Standard library
14. Sandbox and policy
15. Resource limits
16. Embedding API (Rust)
17. Tooling: checker, linter, formatter, test runner, CLI
18. Diagnostics

---

## 1. Design goals _(informative)_

- **Correct by default.** Immutable bindings, explicit mutation (`var` bindings and
  `var` fields), checked integer arithmetic, exhaustive pattern matching, no null,
  errors as values (`Result`, `Option`), and a linter that flags dubious code.
- **Well typed, lightly annotated.** Function signatures are annotated; everything
  inside a function body is inferred.
- **Elegant.** Few keywords, expression-oriented blocks, string interpolation,
  comprehensions, method chaining, pipes, named and default arguments, duration
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

```
// line comment
/* block comment, /* may nest */ */
/// documentation comment for the following item, field, variant or method
//! documentation comment for the enclosing module (only before the first item)
```

Doc comments are retained and are available at runtime through the `help` module.

### 2.3 Line termination

Newlines are significant. The lexer converts a newline into a statement
terminator (`;`) when the last token on the line is one of:

- an identifier, keyword `self`, `true`, `false`, `break`, `continue`, `return`
- a literal (number, duration, string, bytes)
- a closing `)`, `]` or `}`
- the postfix operator `?`

and the newline is **not** inside a `(...)` or `[...]` group. Inside `{...}`
newlines are significant again, even when that brace is nested inside
parentheses. (So `break @outer` ends a statement at the end of a line, and a
loop label `@outer` must be on the same line as its loop, §6.9.)

A terminator is **not** inserted if the first token on the next non-blank line is
`.`, `|>`, `?.`, `else`, `and`, `or`, `=>` or a binary operator other than `-`
and `*`. This permits chains such as:

```
let total = items
    .filter(x => x.price > 0)
    .map(x => x.price)
    .sum()
```

Explicit `;` may be used to separate statements on one line. Redundant
terminators are ignored.

### 2.4 Identifiers and keywords

```
ident = [A-Za-z_][A-Za-z0-9_]*
```

Reserved keywords:

```
and as break continue defer else enum extend false fn for from if impl import
in let loop match native not or pub return self Self spawn struct trait true
type var while
```

`test` is a contextual keyword: it begins a test block only when followed by a
string literal at the top level of a module.

By convention (enforced by the linter): types, traits, enum variants and type
parameters are `UpperCamelCase`; functions, variables, fields, modules and loop
labels are `snake_case`; top-level `let` constants may also be `SCREAMING_SNAKE_CASE`. The identifier `_` is a wildcard and may not be read.

### 2.5 Literals

**Integers**: `0`, `42`, `1_000_000`, `0xff`, `0b1010`, `0o777`. Underscores may
separate digits. The value must fit in a 64-bit signed integer (the literal
`9223372036854775808` is permitted only as the operand of unary minus).

**Floats**: `1.0`, `0.5`, `1e9`, `2.5e-3`, `1_000.5`. A float must have digits
on both sides of a `.`; `1.` and `.5` are not floats.

**Durations**: an integer or float literal immediately followed by a unit:
`ns`, `us`, `ms`, `s`, `m`, `h`. Examples: `250ms`, `1.5s`, `2h`. The value is
stored as a signed 64-bit number of nanoseconds; fractional nanoseconds are
truncated.

**Booleans**: `true`, `false`.

**Strings**: `"..."`. Escapes: `\\`, `\"`, `\n`, `\r`, `\t`, `\0`, `\{`, `\}`,
`\u{XXXX}` (1-6 hex digits, a Unicode scalar value). Any other escape is an
error. Strings may contain literal newlines.

**Interpolation**: inside a non-raw string, `{expr}` evaluates `expr` and
inserts its display form (§10.9). `{expr:spec}` applies a format specification
(§10.10). The expression may contain nested strings and brackets; the format
spec begins at the first `:` that is not nested inside `()`, `[]`, `{}` or a
string.

> **Beware literal braces.** Because every `{` in a non-raw string starts an
> interpolation, text that merely *contains* braces changes meaning silently or
> fails to compile. A regex quantifier such as `"[0-9]{4}"` evaluates `4` and
> produces the pattern `[0-9]4`; JSON text such as `"{\"a\": 1}"` is parsed as
> an interpolation and rejected. Write such text as a raw string
> (`r"[0-9]{4}"`, `r#"{"a": 1}"#`) or escape the braces (`"[0-9]\{4\}"`). The
> linter reports interpolations whose expression is a literal
> (`interpolated-literal`, §17.2), which catches the regex case.

**Multi-line strings**: `"""` starts a block string that ends at the next
`"""`. If the opening `"""` is followed by a newline, that newline is removed.
The common leading whitespace of all non-blank lines (and of the line holding
the closing `"""`) is removed. Escapes and interpolation work as in `"..."`.

**Raw strings**: `r"..."`, `r#"..."#`, `r##"..."##`, ... have no escapes and no
interpolation.

**Bytes**: `b"..."` produces a `bytes` value. Escapes as in strings plus
`\xNN`; no interpolation. Non-ASCII characters are encoded as UTF-8.

### 2.6 Operators and punctuation

```
+  -  *  /  %  **  &  |  ^  ~  <<  >>
== != <  <= >  >=  =  += -= *= /= %=
.. ..= => -> |> ?? ? . , : ; @
( ) [ ] { }
```

---

## 3. Programs, modules and imports

### 3.1 Modules

Every source file is a module. A module consists of an optional module doc
comment, `import` declarations, then any mix of item declarations (§4),
`test` blocks and top-level statements.

When a module is executed, its top-level statements and top-level `let`/`var`
initialisers run in source order. Functions, types and methods may be referred
to before their textual definition. A top-level variable read before its
initialiser has run causes a panic (`global used before initialization`).

The **main module** is the module the host asks the engine to run. Each imported
module is executed exactly once, before the first module that imports it,
following a depth-first order of `import` declarations. Import cycles are a
compile-time error.

A session (`sand repl`, §17.6) is a main module whose items arrive one input
at a time; each input is checked and run when it arrives, and may shadow the
names of earlier inputs.

### 3.2 Imports

```
import json                         // standard or host-native module, bound as `json`
import math as m                    // renamed
import "./geometry.sand"            // script module, bound as `geometry` (file stem)
import "lib/util.sand" as util      // script module, renamed
from math import sqrt, pi           // bind selected public names directly
from "./shapes.sand" import Shape, area
```

Script paths are resolved relative to the directory of the importing module; the
`.sand` extension may be omitted. The resolved path must lie within the
**module root** (the directory of the main module, unless the host configures
otherwise; a symlinked main file's imports and module root are those of the
file it links to). Every script import is checked against the policy as
`Import(path)` (§14).

A module name (`json`) refers first to a host-registered native module, then to a
standard-library module. Only items marked `pub` are visible to importers.
Accessing a non-`pub` item is an error. Importing a name that is not exported is
an error.

A module-qualified name is written `module.name`; a module-qualified type is
written `module.Type`.

### 3.3 Visibility

`pub` may precede `fn`, `struct`, `enum`, `trait`, `type`, `let` and `var` at
the top level. Fields and methods of a public type are public, as are the
methods of a public trait. Impls have no visibility of their own (§4.9).
Top-level `var`s are exported read-only: importers may read but not assign
them.

---

## 4. Declarations

### 4.1 Functions

```
/// Returns the larger of two values.
pub fn max<T: Ord>(a: T, b: T) -> T {
    if a >= b { a } else { b }
}

fn greet(name: str, greeting: str = "Hello") {
    print("{greeting}, {name}!")
}
```

- Every parameter has a type annotation. Parameters may have default values;
  defaults are evaluated at each call that omits them, in the callee's
  module scope, and may refer to earlier parameters. Parameters with defaults
  must follow all parameters without them.
- The return type follows `->`; if omitted it is `()` (unit).
- The body is a block. The value of the block is the return value. In a function
  returning `()`, the value of the final expression is discarded.
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

```
pub struct Point {
    x: float
    y: float

    /// Distance from the origin.
    fn norm(self) -> float {
        math.sqrt(self.x * self.x + self.y * self.y)
    }

    fn origin() -> Point { Point { x: 0.0, y: 0.0 } }
}

struct Counter {
    name: str
    var count: int = 0
}
```

- Fields are separated by newlines or commas. Fields are immutable unless
  declared `var`. A field may have a default value expression which is used when
  a struct literal omits the field.
- Methods are `fn` declarations inside the body. A method whose first parameter
  is `self` (no annotation) is an instance method, called as `value.method(...)`.
  Other methods are static, called as `Type.method(...)`.
- Within the body, `Self` names the struct type (with its type parameters).
- Structs may be generic: `struct Pair<A, B> { first: A, second: B }`.
- Structs have reference semantics: assigning a struct value shares it.

### 4.3 Enums

```
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

```
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

```
type Grid = [[bool]]
type Pair<T> = (T, T)
```

Aliases are transparent: `Grid` and `[[bool]]` are the same type. Aliases may
not be recursive.

### 4.6 Top-level variables

```
let GREETING = "hi"
pub let LIMITS = [1, 2, 3]
var counter = 0
```

Top-level `let` and `var` follow the same rules as local bindings; their types
must be fully inferred from their initialiser and module-level uses.

### 4.7 Native declarations

Inside the prelude, standard library and host native modules, a function may be
declared without a body using `native`:

```
pub native fn parse(text: str) -> Result<Json>
```

The implementation is supplied in Rust by the module's registry. User scripts
may not declare `native` functions. A native type parameter may be marked
`reflect` (`native fn decode<reflect T>(text: str) -> Result<T>`); the compiler
then passes a runtime description of the instantiated type, which must be fully
known at the call site.

### 4.8 Test blocks

```
test "addition is commutative" {
    assert(1 + 2 == 2 + 1)
}
```

Test blocks are ignored by `run` and executed by the test runner (§17.4).

### 4.9 Traits and impls

A **trait** names a set of methods that types can implement. Generic code
bounded by a trait can call its methods.

```
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
    fn describe(self) -> str { "({self.x}, {self.y})" }
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
  type) are required wherever the impl is used: `impl<T: Describe> Describe
  for [T]` makes `[T]` implement `Describe` exactly when `T` does.
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

An inherent method takes precedence over a trait method of the same name.
If several traits provide `m`, the call is ambiguous and an error. Every
trait method can also be called in qualified form:

```
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
| `int`      | 64-bit signed integers                              |
| `float`    | IEEE-754 binary64                                   |
| `bool`     | `true`, `false`                                     |
| `str`      | immutable UTF-8 text                                |
| `bytes`    | immutable byte strings                              |
| `duration` | signed 64-bit nanosecond spans                      |
| `()`       | unit, the single value `()`                         |
| `never`    | the type of expressions that do not produce a value |

`never` is a subtype of every type: an expression of type `never` (`return`,
`break`, `continue`, `panic(...)`) may appear where any type is expected.

### 5.2 Composite types

| Syntax          | Meaning                                                   |
| --------------- | --------------------------------------------------------- |
| `[T]`           | growable list (reference type, mutable)                   |
| `[K: V]`        | hash map, insertion ordered (reference type, mutable)     |
| `Set<T>`        | hash set, insertion ordered (reference type, mutable)     |
| `(A, B, ...)`   | tuple of two or more elements (immutable)                 |
| `T?`            | shorthand for `Option<T>`                                 |
| `Result<T, E>`  | `Ok(T)` or `Err(E)`; `Result<T>` means `Result<T, Error>` |
| `fn(A, B) -> R` | function; `fn(A)` returns `()`                            |
| `Task<T>`       | a concurrently running computation (§11)                  |
| `Channel<T>`    | a message queue between tasks (§11)                       |
| `Range`         | an integer range `a..b` / `a..=b`                         |

### 5.3 Nominal types

Structs and enums are nominal. Two struct types are equal only if they are the
same declaration with equal type arguments.

### 5.4 Built-in nominal types

The prelude defines:

```
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
constructed with a literal, and are manipulated only by methods.

### 5.5 Type equality and subtyping

Apart from `never`, there is no subtyping and there are no implicit conversions.
In particular `int` and `float` never convert implicitly: `1 + 2.0` is a type
error. Use `x.to_float()` or `f.to_int()`.

### 5.6 Bounds

| Bound  | Satisfied by                                                                                                                                                   |
| ------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Eq`   | every type except function types (and types containing them)                                                                                                   |
| `Ord`  | `int`, `float`, `bool`, `str`, `bytes`, `duration`, `()`, and tuples, lists, `Option`, structs and enums all of whose components are `Ord`                     |
| `Hash` | `int`, `bool`, `str`, `bytes`, `duration`, `()`, and tuples, `Option`, `Result`, enums and structs **without `var` fields** all of whose components are `Hash` |
| `Num`  | `int`, `float`                                                                                                                                                 |

Map keys and set elements must be `Hash`. Built-in bounds are satisfied
structurally and cannot be implemented. Traits (§4.9) are user-defined bounds;
a trait whose supertraits include a built-in bound implies it.

A type parameter bounded by `Num` (directly, in a `where` clause, or through a
trait with `Num` as a supertrait) also has the static methods `T.zero()` and
`T.one()`, the additive and multiplicative identities (`0`/`1` for `int`,
`0.0`/`1.0` for `float`). They are available even when no value of type `T`
exists, so generic code can, for example, sum an empty list:

```
fn total<T: Num>(xs: [T]) -> T {
    xs.fold(T.zero(), (a, b) => a + b)
}
let none: [float] = []
print(total(none))            // 0.0
```

(Runtime values do not carry their static types, so this information is
passed to generic code as hidden evidence, like trait dictionaries.)

---

## 6. Expressions

### 6.1 Precedence

From lowest to highest binding power:

| Level | Operators                                            | Associativity |
| ----- | ---------------------------------------------------- | ------------- |
| 1     | `\|>`                                                | left          |
| 2     | `??`                                                 | right         |
| 3     | `or`                                                 | left          |
| 4     | `and`                                                | left          |
| 5     | `not` (prefix)                                       | —             |
| 6     | `==` `!=` `<` `<=` `>` `>=`                          | none          |
| 7     | `\|`                                                 | left          |
| 8     | `^`                                                  | left          |
| 9     | `&`                                                  | left          |
| 10    | `<<` `>>`                                            | left          |
| 11    | `..` `..=`                                           | none          |
| 12    | `+` `-`                                              | left          |
| 13    | `*` `/` `%`                                          | left          |
| 14    | `-` `~` (prefix)                                     | —             |
| 15    | `**`                                                 | right         |
| 16    | postfix: call, `.field`, `.method()`, `[index]`, `?` | left          |

Comparison operators do not chain: `a < b < c` is a syntax error. `-2 ** 2` is
`-(2 ** 2)`.

### 6.2 Literals and names

Literals have their obvious types. A name refers to the innermost binding in
scope: local variable, parameter, local function, top-level item of the current
module, imported name, or prelude item.

### 6.3 Collections

```
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
aliased). `n` must be an `int`; a negative `n` panics.

Comprehensions may have several `for` clauses and `if` filters, evaluated left
to right as nested loops: `[(x, y) for x in xs for y in ys if x != y]`.

### 6.4 Struct literals

```
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
- `duration + duration`, `duration - duration`, `-duration`,
  `duration * int`, `duration / int` produce `duration`;
  `duration / duration` produces `float`.

Integer semantics:

- Overflow in `+ - * **`, unary `-`, and `/` panics with `integer overflow`.
- `/` is **floor division** and `%` is **floor modulo**: `-7 / 2 == -4`,
  `-7 % 2 == 1`, `7 % -2 == -1`. Division or modulo by zero panics with
  `division by zero`.
- `a ** b` with `b < 0` panics with `negative exponent`.

Float semantics: IEEE-754 with round-to-nearest. `/` by zero yields `inf`,
`-inf` or `NaN`. `%` is floor modulo (`a - b * floor(a / b)`). `**` is `powf`.

**Bitwise** `& | ^ << >> ~` operate on `int`. `>>` is arithmetic. A shift
amount outside `0..64` panics with `shift out of range`.

**Comparison** `== !=` require `Eq` operands of the same type and compare
structurally. `< <= > >=` require `Ord` operands of the same type. Ordering is:
numeric for numbers (NaN compares false with everything); lexicographic by
bytes for `str` and `bytes`; `false < true`; lexicographic for tuples and lists;
for enums, by variant declaration order then payload; for structs, field by
field in declaration order. Floats inside composites use the total order where
`NaN` is greater than every other float and equal to itself.

Values of reference types (lists, maps, sets, structs) compare **by
content**. Tasks, channels and opaque handles compare by identity.

**Logical** `and`, `or`, `not` take and return `bool`; `and`/`or` short-circuit.

**Range** `a..b` (exclusive) and `a..=b` (inclusive) require `int` operands and
produce a `Range`.

**Pipe** `x |> f` is `f(x)`; `x |> f(a, b)` is `f(x, a, b)`; `x |> obj.m(a)` is
`obj.m(x, a)`. The right-hand side must be a name, a qualified name or a call.

**Coalesce** `a ?? b`: if `a: T?`, yields the payload of `Some` or else
evaluates `b: T`. If `a: Result<T, E>`, yields the `Ok` payload or else `b`.

### 6.6 Calls

```
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
- `xs[i]` indexes a list (panics `index out of bounds` unless `0 <= i < len`).
- `m[k]` looks up a map key (panics `key not found` if absent).
- `s[i]` on `str` and `bytes` is not permitted; use slices or methods.
- `xs[a..b]`, `xs[a..]`, `xs[..b]`, `xs[a..=b]` produce a new list. On `str`
  and `bytes`, slices produce new values and indices are **byte** offsets. A
  slice bound out of range, `a > b`, or a `str` bound that is not on a
  character boundary panics.

### 6.8 Option and error propagation

- Postfix `e?` where `e: T?` evaluates to the `Some` payload or returns `None`
  from the enclosing function, whose return type must be an `Option`.
- Postfix `e?` where `e: Result<T, E>` evaluates to the `Ok` payload or returns
  `Err(err)` from the enclosing function, whose return type must be
  `Result<_, E>` with the same `E`.
- At the top level of the main module, `e?` on `Err(err)` aborts the program
  with exit code 1 and reports the error. At the top level of an imported module
  it is an error to use `?`.
- `?` is not permitted inside a lambda unless the lambda's return type is an
  appropriate `Option`/`Result`.

### 6.9 Blocks and control flow expressions

```
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
- `for` iterates: `Range` (integers), `[T]` (elements), `[K: V]` (`(K, V)`
  tuples), `Set<T>` (elements), `Channel<T>` (received messages until the
  channel is closed and empty). The loop binding may be a pattern:
  `for (k, v) in map`. A `for` over a list visits indices `0..n` where `n` is
  the length when the loop starts (stopping early if the list shrinks), reading
  each element when it is visited; so elements pushed during the loop are not
  visited. Maps and sets are iterated over a snapshot of their entries taken
  when the loop starts. Channels are consumed live. `for` also iterates over
  any value whose type implements the prelude trait `Iterator<T>` (§12.13),
  such as the lazy `Iter<T>`, calling its `next()` before each iteration until
  it returns `None` (the call is static for concrete types and goes through
  the dictionary for a bounded type parameter). Comprehension `for` clauses
  accept the same values.
- `break` and `continue` refer to the innermost loop. `return e` returns from
  the innermost function or lambda. All three have type `never`.
- **Labels.** A loop may be labeled by writing `@name` before `loop`, `while`
  or `for`, on the same line; `break @name` and `continue @name` then refer to
  that loop from anywhere in its body, including nested loops:

  ```
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

  `break @name value` is allowed only when `@name` labels a `loop` (whose
  value it gives, as for an unlabeled `break`); `continue @name` may target any
  loop (for `loop`, it restarts the body). Labels name loops of the current
  function or lambda only: `break` and `continue` cannot leave a lambda or a
  local function, labeled or not. Using an unknown label, or a label that is
  already used by an enclosing loop of the same function, is a compile error.
  Labels are not values and have no other use. The label must directly follow
  the `@` (no space).

### 6.10 Lambdas

```
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

`"{a} + {b} = {a + b}"` builds a string from the display forms (§10.9) of the
embedded expressions. Any type may be interpolated.

### 6.12 `spawn`

`spawn e` evaluates expression `e` in a new task and returns a `Task<T>` where
`T` is the type of `e`. Variables are captured as for lambdas. See §11.

---

## 7. Statements

```
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

```
fn copy(src: str, dst: str) -> Result<()> {
    let conn = net.connect(dst)?
    defer conn.close()                  // runs on every exit below
    for line in fs.read_lines(src)? {
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
  module's top-level code finishes (or when a top-level `?` stops the
  program).
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
- **Panics and cancellation.** A panic aborts the run (§10.3) without running
  defers, as does `exit`. A cancelled task (§11.2) stops at a suspension point
  and does not run its pending defers; nor does a task cut short because the
  run ends. Deferred code may itself suspend (perform I/O, wait on tasks or
  channels) like any other code.

---

## 8. Patterns

### 8.1 Syntax

```
_                     wildcard
name                  binding
42  -1  "text"  true  literal (int, str, bool)
1..=9                 inclusive int range
(p1, p2)              tuple
Some(p)  None         Option variants
Ok(p)  Err(p)         Result variants
Circle(p)             enum variant (enum inferred from scrutinee)
Shape.Circle(p)       qualified variant
Point { x, y: 0 }     struct (fields not mentioned are ignored with `..`)
Point { x, .. }
[]  [a, b]  [first, ..rest]  [.., last]   list patterns
p1 | p2               alternatives (must bind the same names with the same types)
```

A binding pattern `name` that coincides with a payload-less variant of the
scrutinee's enum type refers to that variant (so `Empty` in the example above
is a variant, not a binding). Struct patterns must either mention every field
or end with `..`.

### 8.2 Guards

`pattern if cond => body` matches only when `cond` is true. Guards do not count
towards exhaustiveness.

### 8.3 Exhaustiveness and reachability

A `match` must be exhaustive: for every value of the scrutinee's type some arm
must match. Types with infinitely many values (`int`, `str`, lists, ...) require
a wildcard or binding arm (list patterns `[]` + `[_, .._]` are exhaustive). A
non-exhaustive match is a compile error that lists a missing pattern. An arm
that can never match because earlier arms cover it is a compile error
(`unreachable pattern`). Checking is exponential in the worst case (e.g.
or-patterns in many tuple positions); a `match` (or refutability check of a
`let`, `for` or comprehension pattern) whose check exceeds a fixed amount of
work is rejected with `pattern too complex to check for exhaustiveness`
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
  candidate methods. If the type is a struct with a function-typed field named
  `m`, `e.m(...)` calls the field's value; otherwise an inherent method (declared
  in the type or an `extend` block) is used; otherwise a trait method (§4.9),
  which for a type parameter comes from its bounds. Every type also has the
  universal methods `to_str() -> str` (display form) and `debug() -> str`
  (debug form), unless a trait method of the same name applies.
- Integer and float literals have types `int` and `float` respectively; there
  is no literal polymorphism.
- At the end of checking a function, every type variable must be resolved,
  except those bound to the function's own type parameters. Unresolved
  variables are errors (`cannot infer type`).
- Bounds on type parameters are checked at instantiation. Trait bounds are
  checked once the function's types are inferred: each needs an impl (§4.9),
  a bound of an enclosing type parameter, or `Self` in a trait.
- An expression of type `never` unifies with any type.
- Every type, after inference variables are resolved, has at most 10,000
  components (type constructors and leaves); larger types, which can only
  arise by doubling (`let b = (a, a)` repeatedly, or similar aliases or
  generic calls), are rejected with `type too large`. See §15.3 for the other
  compile-time limits.

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

### 10.3 Panics

A panic aborts the whole engine run (all tasks) with a runtime error carrying a
message and a stack trace of `file:line` entries. Panics are raised by: integer
overflow, division by zero, index/key/slice errors, `panic(msg)`, failed
`assert`, `unwrap`/`expect` on `None`/`Err`, unreachable code, use of a global
before initialisation, stack overflow, deep nesting during value printing,
comparison or hashing (beyond 2,000 levels), and waiting on a cancelled task.
Panics cannot be caught by scripts and do not run deferred expressions
(§7.1). (The test runner isolates panics per test.)

### 10.4 Exit

`exit(code: int) -> never` stops the engine run immediately with the given exit
code. It does not terminate the host process.

### 10.5 Functions and closures

Calls are by value (references for reference types). Recursion depth is bounded
by the `max_call_depth` limit (default 10,000); exceeding it panics with
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

Unreachable heap values are reclaimed by a tracing collector. Collection is
not observable except through memory limits and the closing of unreachable
resources (files, sockets) which are closed when collected.

### 10.9 Display and debug forms

`to_str()` (display form; used by `print` and interpolation) and `debug()`
(debug form; used inside composite values) are:

| Value                               | Display                             | Debug                                                                           |
| ----------------------------------- | ----------------------------------- | ------------------------------------------------------------------------------- |
| `42`                                | `42`                                | `42`                                                                            |
| `1.0`, `0.1`, `1e100`, `NaN`, `inf` | `1.0`, `0.1`, `1e100`, `NaN`, `inf` | same                                                                            |
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
| `Error`                             | `{kind}: {message}`                 | `Error { kind: "...", message: "..." }`                                         |
| function                            | `<fn name>`                         | same                                                                            |
| task / channel                      | `<task>` / `<channel>`              | same                                                                            |
| `0..3`                              | `0..3`                              | same                                                                            |

Composite values print their components in debug form. Durations display with
the largest unit among `h`, `m`, `s`, `ms`, `us`, `ns` that keeps the value
`>= 1`, using the shortest float representation of the value in that unit:
`90s` displays as `1.5m`, `1500ms` as `1.5s`, `0ns` as `0s`.

### 10.10 Format specifications

`{expr:spec}` with `spec = [[fill]align][sign][0][width][.precision][type]`:

- `align`: `<` left, `>` right, `^` center. Default: right for numbers, left
  otherwise. `fill` is any single character (default space).
- `sign`: `+` forces a sign for numbers.
- `0`: zero-pad numbers after the sign to `width`.
- `width`: minimum width in characters.
- `.precision`: digits after the decimal point for floats; maximum characters
  for strings.
- `type`: `x` / `X` hex, `b` binary, `o` octal (ints only); `e` scientific
  (floats); `?` debug form.

Examples: `"{3.14159:.2}"` → `3.14`; `"{42:>5}"` → `   42`; `"{7:03}"` →
`007`; `"{255:x}"` → `ff`; `"{"ab":*^6}"` → `**ab**`; `"{"hi":?}"` → `"hi"`.

---

## 11. Concurrency and asynchronous I/O

### 11.1 Model

An engine runs one task at a time on one OS thread. All I/O is asynchronous:
when a task performs I/O it is suspended and other ready tasks run. There is no
`async`/`await` colouring — any function may suspend. Tasks are scheduled
cooperatively with preemption at instruction-count boundaries, so a busy loop in
one task does not starve others.

When the main module's top-level code finishes, the run ends: unfinished tasks
are cancelled. If every task is blocked and no I/O or timer is pending, the run
fails with `deadlock: all tasks are blocked`.

### 11.2 Tasks

```
let t = spawn fetch(url)          // Task<Result<Response>>
let r = t.wait()                  // suspends until done
t.is_done()                       // bool
t.cancel()                        // cancels if still running
```

- `spawn e` creates a task which starts running at the next scheduling point.
- `t.wait()` returns the task's result. Waiting on a cancelled task panics
  with `task cancelled`. Waiting on a task more than once returns the same
  value.
- Cancelling a task stops it at its next suspension point or instruction
  boundary; pending I/O is dropped and its pending deferred expressions
  (§7.1) do not run.
- A panic in any task aborts the run (§10.3).

### 11.3 The `task` module (prelude-visible as `task`)

| Function                                           | Description                                                         |
| -------------------------------------------------- | ------------------------------------------------------------------- |
| `task.sleep(d: duration)`                          | suspend for `d`                                                     |
| `task.yield_now()`                                 | let other tasks run                                                 |
| `task.all<T>(ts: [Task<T>]) -> [T]`                | wait for all, results in order                                      |
| `task.race<T>(ts: [Task<T>]) -> T`                 | wait for the first to finish; cancel the rest; panics on empty list |
| `task.timeout<T>(d: duration, f: fn() -> T) -> T?` | run `f` in a task; `None` (and cancel) if not done within `d`       |
| `task.channel<T>(capacity: int = 0) -> Channel<T>` | create a channel; `0` means unbounded                               |

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
resumes.

---

## 12. The prelude

Always in scope without import.

### 12.1 Functions

| Function                                            | Description                           |
| --------------------------------------------------- | ------------------------------------- |
| `print<T>(x: T)`                                    | write display form + `"\n"` to stdout |
| `eprint<T>(x: T)`                                   | write display form + `"\n"` to stderr |
| `panic(msg: str) -> never`                          | panic                                 |
| `assert(cond: bool, msg: str = "assertion failed")` | panic with `msg` if false             |
| `todo() -> never`                                   | panic `not yet implemented`           |
| `exit(code: int) -> never`                          | stop the run (§10.4)                  |
| `error(message: str, kind: str = "error") -> Error` | construct an `Error`                  |
| `set<T: Hash>(items: [T] = []) -> Set<T>`           | build a set                           |

### 12.2 Universal methods

Every value has `to_str() -> str` and `debug() -> str`.

### 12.3 `int`

`abs`, `pow(e: int) -> int`, `min(o)`, `max(o)`, `clamp(lo, hi)`, `to_float`,
`to_str_radix(radix: int) -> str` (2..36), `sign() -> int`, `is_even`, `is_odd`,
`to_char() -> str?` (Unicode scalar to one-char string). Static:
`int.parse(s: str, radix: int = 10) -> int?`, `int.max_value() -> int`,
`int.min_value() -> int`.

### 12.4 `float`

`abs`, `floor`, `ceil`, `round` (half away from zero), `trunc`, `sqrt`, `pow(e:
float)`, `min`, `max`, `clamp`, `is_nan`, `is_finite`, `is_infinite`,
`to_int() -> int` (truncates; panics on NaN or out of range), `to_str_fixed(digits:
int) -> str`. Static: `float.parse(s: str) -> float?`.

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
| `split(sep: str) -> [str]`                                                         | `"".split(",") == [""]`; empty `sep` panics                                           |
| `split_once(sep: str) -> (str, str)?`                                              |                                                                                       |
| `split_whitespace() -> [str]`                                                      |                                                                                       |
| `lines() -> [str]`                                                                 | splits on `\n`, strips a trailing `\r` from each line, no final empty line            |
| `trim()`, `trim_start()`, `trim_end()`                                             | whitespace                                                                            |
| `strip_prefix(p: str) -> str?`, `strip_suffix(s: str) -> str?`                     |                                                                                       |
| `to_upper()`, `to_lower()`                                                         | Unicode-aware                                                                         |
| `repeat(n: int) -> str`                                                            |                                                                                       |
| `pad_start(width: int, fill: str = " ")`, `pad_end(...)`                           | pad to `width` characters                                                             |
| `parse_int() -> int?`, `parse_float() -> float?`                                   | strict (no whitespace)                                                                |
| `is_digit()`, `is_alpha()`, `is_alnum()`, `is_space()`, `is_upper()`, `is_lower()` | true if non-empty and all chars satisfy (ASCII classes for digit; Unicode for others) |
| `char_at(i: int) -> str?`                                                          | the scalar starting at byte offset `i`                                                |
| `ord() -> int`                                                                     | scalar value of a one-char string (panics otherwise)                                  |
| `reversed() -> str`                                                                | by scalars                                                                            |
| `join(items: [str]) -> str`                                                        | `", ".join(xs)`                                                                       |
| `chars_iter() -> Iter<str>`                                                        | lazy iterator over the scalars (§12.13)                                               |

`[str].join(sep: str = "")` is also available on lists.

### 12.6 `bytes`

`len`, `is_empty`, `get(i) -> int?`, `to_list() -> [int]`, `to_str() -> Result<str>`
(strict UTF-8; this shadows the universal `to_str`, use `debug()` for display),
`to_str_lossy() -> str`, `to_hex() -> str`, `contains(b: bytes)`, `find(b: bytes)
-> int?`, slicing. Static: `bytes.from_list(xs: [int]) -> bytes` (panics if any
element outside `0..=255`), `bytes.from_hex(s: str) -> bytes?`.

### 12.7 `duration`

`nanos() -> int`, `micros() -> int`, `millis() -> int`, `secs() -> float`,
`abs()`. Static: `duration.from_nanos(n: int)`, `duration.from_millis(n: int)`,
`duration.from_secs(s: float)`.

### 12.8 `[T]`

| Method                                                                          | Description                   |
| ------------------------------------------------------------------------------- | ----------------------------- |
| `len()`, `is_empty()`                                                           |                               |
| `push(x: T)`, `pop() -> T?`                                                     | at the end                    |
| `insert(i: int, x: T)`, `remove(i: int) -> T`                                   | panics if out of range        |
| `extend(xs: [T])`, `clear()`                                                    |                               |
| `get(i: int) -> T?`, `first() -> T?`, `last() -> T?`                            |                               |
| `contains(x: T) -> bool`, `index_of(x: T) -> int?`                              | `T: Eq`                       |
| `reverse()` (in place), `reversed() -> [T]`                                     |                               |
| `sort()`, `sorted() -> [T]`                                                     | `T: Ord`, stable              |
| `sort_by<K: Ord>(key: fn(T) -> K)`, `sorted_by<K: Ord>(key: fn(T) -> K) -> [T]` | stable                        |
| `map<U>(f: fn(T) -> U) -> [U]`                                                  |                               |
| `filter(f: fn(T) -> bool) -> [T]`                                               |                               |
| `filter_map<U>(f: fn(T) -> U?) -> [U]`                                          |                               |
| `flat_map<U>(f: fn(T) -> [U]) -> [U]`                                           |                               |
| `each(f: fn(T))`                                                                |                               |
| `fold<A>(init: A, f: fn(A, T) -> A) -> A`                                       |                               |
| `reduce(f: fn(T, T) -> T) -> T?`                                                |                               |
| `any(f: fn(T) -> bool)`, `all(f: fn(T) -> bool)`                                |                               |
| `find(f: fn(T) -> bool) -> T?`, `find_index(f: fn(T) -> bool) -> int?`          |                               |
| `count(f: fn(T) -> bool) -> int`                                                |                               |
| `sum() -> T`, `product() -> T`                                                  | `T: Num`; empty: `T.zero()` / `T.one()` |
| `min() -> T?`, `max() -> T?`                                                    | `T: Ord`                      |
| `min_by<K: Ord>(key: fn(T) -> K) -> T?`, `max_by<K: Ord>(...)`                  | first minimal / first maximal |
| `enumerate() -> [(int, T)]`                                                     |                               |
| `zip<U>(other: [U]) -> [(T, U)]`                                                | truncates to the shorter      |
| `take(n: int)`, `skip(n: int)`                                                  | clamp to length               |
| `take_while(f)`, `skip_while(f)`                                                |                               |
| `chunks(n: int) -> [[T]]`, `windows(n: int) -> [[T]]`                           | `n > 0`                       |
| `dedup() -> [T]`                                                                | remove consecutive duplicates |
| `unique() -> [T]`                                                               | first occurrences, `T: Hash`  |
| `group_by<K: Hash>(key: fn(T) -> K) -> [K: [T]]`                                |                               |
| `partition(f: fn(T) -> bool) -> ([T], [T])`                                     |                               |
| `copy() -> [T]`                                                                 | shallow copy                  |
| `swap(i: int, j: int)`                                                          |                               |
| `join(sep: str = "") -> str`                                                    | `T = str` only                |
| `to_set() -> Set<T>`                                                            | `T: Hash`                     |
| `iter() -> Iter<T>`                                                             | lazy iterator (§12.13)        |

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

Option: `is_some`, `is_none`, `unwrap() -> T` (panics `unwrap on None`),
`expect(msg: str) -> T`, `unwrap_or(d: T) -> T`, `map<U>(f) -> U?`,
`and_then<U>(f: fn(T) -> U?) -> U?`, `or_else(f: fn() -> T?) -> T?`,
`ok_or<E>(e: E) -> Result<T, E>`, `filter(f: fn(T) -> bool) -> T?`.

Result: `is_ok`, `is_err`, `unwrap() -> T` (panics `unwrap on Err: <err>`),
`expect(msg: str) -> T`, `unwrap_or(d: T) -> T`, `unwrap_err() -> E`,
`map<U>(f) -> Result<U, E>`, `map_err<F>(f: fn(E) -> F) -> Result<T, F>`,
`and_then<U>(f: fn(T) -> Result<U, E>) -> Result<U, E>`, `ok() -> T?`,
`err() -> E?`.

### 12.12 `Range`

`start() -> int`, `end() -> int` (exclusive), `len() -> int`, `contains(i: int)
-> bool`, `to_list() -> [int]`, `map`, `filter`, `each`, `step_by(n: int) -> [int]`,
`reversed() -> [int]`, `iter() -> Iter<int>` (§12.13).

### 12.13 Iterators

The methods of lists, maps and ranges above are **eager**: `xs.map(f)` calls
`f` on every element at once and builds a new list. **Lazy iterators** do the
work only when elements are requested, one at a time, so pipelines build no
intermediate lists, can stop early, and can draw from endless sources:

```
let firsts = Iter.count_from(1).map(n => n * n).filter(n => n % 3 == 0).take(3)
print(firsts.to_list())                       // [9, 36, 81]
for (i, line) in text.lines().iter().enumerate().skip(1) { ... }
```

**The trait.** The prelude declares

```
pub trait Iterator<T> {
    fn next(self) -> T?                 // the next element; None once exhausted
    fn iter(self) -> Iter<T> { ... }    // a lazy Iter over the remaining elements
}
```

`for` loops and comprehensions iterate over any value whose type implements
`Iterator<T>` (§6.9). User types implement it like any trait (§4.9):

```
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

| Source                                                     | Elements                                                |
| ---------------------------------------------------------- | ------------------------------------------------------- |
| `Iter.from_fn(f: fn() -> T?)`                              | the results of `f` until it returns `None` (also `Iter { next: f }`) |
| `Iter.empty()`                                             | none                                                    |
| `Iter.once(x: T)`                                          | `x`                                                     |
| `Iter.repeat(x: T)`                                        | `x` forever                                             |
| `Iter.count_from(start: int, step: int = 1) -> Iter<int>`  | `start`, `start + step`, ... forever                    |
| `Iter.successors(first: T?, f: fn(T) -> T?)`               | `first`, then `f` of the previous element until `None`  |

**Adapters** return a new `Iter` and do no work until it is consumed:
`map<U>(f: fn(T) -> U)`, `filter(f: fn(T) -> bool)`, `filter_map<U>(f: fn(T) ->
U?)`, `flat_map<U>(f: fn(T) -> Iter<U>)`, `enumerate() -> Iter<(int, T)>`,
`zip<U>(other: Iter<U>) -> Iter<(T, U)>` (ends with the shorter), `chain(other:
Iter<T>)`, `take(n: int)` (pulls at most `n` elements from its source),
`skip(n: int)`, `take_while(f)`, `skip_while(f)`, `step_by(n: int)` (the first
element, then every `n`-th; panics if `n <= 0`), `inspect(f: fn(T))` (calls `f`
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
  elements. Use the list (or call `iter()` again) to iterate twice.
- **Exhaustion.** The iterators of lists, ranges, maps, sets, strings and
  channels keep returning `None` once they have; `Iter.from_fn` iterators
  return whatever their function returns. Whether an adapter asks an
  exhausted source for more elements (`skip` may) is unspecified.
- **Mutation.** A list iterator behaves like a `for` loop (§6.9): it visits
  the indices below the length the list had when `iter()` was called, reading
  each element when it is reached, and stops early if the list shrinks;
  elements pushed meanwhile are not visited. The iterators of maps and sets
  work on a snapshot of the entries taken by `iter()`. A channel iterator
  receives live (and suspends while the channel is empty).
- **Limits.** Iterators are ordinary Sand code: every element costs at least
  one unit of fuel (§15.2), so endless iterators are bounded by the
  instruction, time and memory limits (§15) like any loop.
- **Performance.** A pipeline written as one expression where it is consumed
  (`for x in xs.iter().map(f).filter(g) { ... }`, `xs.iter().map(f).sum()`)
  is compiled to a single loop with the same behaviour, so it builds no
  intermediate lists and costs about as much as the equivalent hand-written
  loop; so are the eager methods taking a function (`xs.map(f)`). An
  iterator stored in a variable and consumed later calls a closure per
  adapter and element.
- **Cost** _(informative)_. Each adapter adds about one function call per
  element besides the user function it applies, so eager list methods are
  usually faster per element; iterators avoid building intermediate lists and
  can stop early.

---

## 13. Standard library

Each module is imported with `import <name>`. Functions that perform I/O return
`Result` values with `Error` kinds from §5.4 and are subject to the policy
(§14). Unless stated, a permission denial produces
`Err(Error { kind: "permission_denied", ... })`.

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
directory (configured by the host; default: the module root).

| Function                                                                         | Capability                        |
| -------------------------------------------------------------------------------- | --------------------------------- |
| `read_text(path) -> Result<str>`                                                 | `FsRead`                          |
| `read_bytes(path) -> Result<bytes>`                                              | `FsRead`                          |
| `read_lines(path) -> Result<[str]>`                                              | `FsRead`                          |
| `write_text(path, text: str) -> Result<()>`                                      | `FsWrite`                         |
| `write_bytes(path, data: bytes) -> Result<()>`                                   | `FsWrite`                         |
| `append_text(path, text: str) -> Result<()>`                                     | `FsWrite`                         |
| `exists(path) -> Result<bool>`                                                   | `FsRead`                          |
| `is_file(path)`, `is_dir(path) -> Result<bool>`                                  | `FsRead`                          |
| `list_dir(path) -> Result<[str]>` (sorted names)                                 | `FsRead`                          |
| `create_dir(path, recursive: bool = false) -> Result<()>`                        | `FsWrite`                         |
| `remove_file(path)`, `remove_dir(path, recursive: bool = false)`                 | `FsWrite` (entry)                 |
| `rename(from, to) -> Result<()>`                                                 | `FsWrite` on both (entries)       |
| `copy(from, to) -> Result<()>`                                                   | `FsRead` on from, `FsWrite` on to |
| `metadata(path) -> Result<Metadata>`                                             | `FsRead`                          |
| `join(parts: [str]) -> str`, `basename(p)`, `dirname(p)`, `extension(p) -> str?` | none                              |

`struct Metadata { size: int, is_file: bool, is_dir: bool, modified: time.DateTime? }`.

Paths may use `/` as the separator on every platform (on Windows `\` is also
a separator). `join` inserts the platform's separator (`\` on Windows) and a
part that is an absolute path replaces the parts before it; `basename`,
`dirname` and `extension` are purely lexical. Paths in capabilities and
prompts are absolute and normalised (§14.1); on Windows they use the plain
`C:\...` form rather than the `\\?\` form. Permission errors name the path
as the script spelled it (`permission denied: read data/x.txt`), so that
they reveal neither symlink targets nor the stored spelling of names the
script may not access. A path longer than 4096 bytes, or containing a NUL
character, fails with kind `"invalid"` before anything else is done.

Symbolic links (§14.1):

- Most functions follow symlinks: the path is resolved first and the policy
  is checked for the resolved location (the link's target), which the
  operation then accesses **without following any symlink**. If a symlink
  appears in the path between the check and the operation, or the path names
  a dangling symlink (one whose target did not exist when the path was
  resolved), the operation fails with kind `"permission_denied"`;
  `exists`, `is_file` and `is_dir` return `false` instead.
- `remove_file`, `remove_dir` and `rename` act on the directory entry
  itself: a final symlink is removed or renamed, never its target, and the
  capability ("entry") is checked for the link's own path (its directory
  resolved), so no permission for the target is needed.
- `rename` fails with `"permission_denied"` if a deny rule of the policy
  protects a path inside the tree being moved or the tree being replaced at
  `to`; `remove_dir(path, recursive: true)` checks the same for `path`
  before removing anything, and checks each entry before removing it.

On Windows, a junction counts as a symlink, and file names that Win32
would alter or treat specially (ending in `.` or a space, containing `:`,
which names an alternate data stream, or a reserved device name such as
`CON` or `nul.txt`) fail with kind `"invalid"`.

Reading, writing and copying accept regular files only (kind `"invalid"`
otherwise, e.g. for directories and FIFOs). `copy` copies the contents
only: a new file gets the default permissions (never the source's execute
or setuid bits). `create_dir(path, recursive: true)` checks `FsWrite` for
every directory it creates, not just `path`, and creates none of the
directories whose existence was assumed when it checked.

Hard links are not symlinks: a hard link to an outside file that someone
else placed inside an allowed directory is the same file, and accessing it
is allowed. The engine cannot tell such links apart from ordinary files in
general; do not allow access to directories that untrusted parties can write
hard links into.

### 13.4 `help`

`doc<T>(f: T) -> str` (doc comment of a function value, `""` if none),
`signature<T>(f: T) -> str` (e.g. `fn(text: str) -> Result<Json, Error>`),
`module(name: str) -> str?` (summary listing of a standard/native module's public
items with signatures and first doc lines), `modules() -> [str]`.

### 13.5 `http`

```
pub struct Response { status: int, headers: [str: str], body: bytes, url: str }
pub fn get(url: str, headers: [str: str] = [:], timeout: duration = 30s) -> Result<Response>
pub fn request(method: str, url: str, headers: [str: str] = [:], body: bytes = b"",
               timeout: duration = 30s, follow_redirects: bool = true) -> Result<Response>
pub fn post(url: str, body: bytes, headers: [str: str] = [:], timeout: duration = 30s) -> Result<Response>
```

Response methods: `text() -> Result<str>`, `json() -> Result<json.Json>`,
`ok() -> bool` (200..=299), `header(name: str) -> str?` (case-insensitive).
Header names in `headers` are lower-cased. HTTP/1.1 over TCP; `https` URLs use
TLS when the engine is built with the `tls` feature (default). Redirects
(301, 302, 303, 307, 308) are followed up to 10 times; each hop is checked
against the policy. Responses with `Content-Length`, chunked encoding, or
read-until-close bodies are supported; bodies larger than the
`max_http_body` limit fail with kind `"limit"`. Non-2xx statuses are **not**
errors. Capability: `Net(host:port)`, then `NetAddr` for the resolved
addresses (§14.6), for every hop.

Server:

```
pub fn serve(addr: str) -> Result<Server>        // capability NetListen(addr)
pub struct Server { listener: net.Listener, pending: PendingRequests }
server.accept() -> Result<Request>
server.addr() -> str
server.close()
pub struct Request { method: str, path: str, query: str, headers: [str: str], body: bytes, conn: net.Conn }
request.respond(status: int, body: bytes, headers: [str: str] = [:]) -> Result<()>
request.text() -> Result<str>
request.json() -> Result<json.Json>
request.header(name: str) -> str?                 // case-insensitive
```

Each connection carries one request (no keep-alive). While `accept` waits,
it accepts connections and reads their requests concurrently (up to 64
connections at a time, and at most half of the sockets `max_open_resources`
still allows), returning the first request that is complete, so idle or
slow clients do not delay others. Connections whose requests are being read
count against `max_open_resources`; reading continues with the next
`accept` call, and `close` closes them. `accept` answers connections that
do not deliver a valid request — malformed, not complete within 30 seconds
of being accepted, or with a body larger than `max_http_body` — with
`400 Bad Request` and closes them. `pending` (of a private type) holds the
connections being read; a `Server` is only obtained from `serve`. `path` and `query`
are the parts of the request target before and after `?`; header names are
lower-cased.

`conn` is the client connection the request arrived on (fields of public
types are public, §3.3). `respond` writes a response with `content-length`
and `connection: close` headers (and no body for `HEAD` requests or statuses
without one) to `conn` and then closes it; a script may instead write to
`conn` itself, e.g. to stream a response, in which case it produces the
HTTP framing and closes the connection. Constructing a `Request` value
directly is allowed but has no special meaning.

### 13.6 `io`

`read_line() -> Result<str?>` (stdin, without the line terminator; `None` at
EOF; capability `Stdin`), `read_all() -> Result<str>` (`Stdin`), `write(s: str)`
(stdout, no newline), `ewrite(s: str)` (stderr), `flush()`.

The CLI's standard output is buffered like C's stdio: by line on a
terminal, in blocks otherwise. It is flushed by `flush()`, at the end of the
run, before writing to standard error, before a permission prompt, whenever
every task is waiting (for I/O, a timer, ...), and, on a terminal, before
reading standard input. Standard error is not buffered.

### 13.7 `env`

`get(name: str) -> str?` (capability `Env(name)`; a denial yields `None`),
`args() -> [str]` (arguments the host passed to the script; no capability),
`vars() -> [str: str]` (capability `Env("*")`).

### 13.8 `json`

```
pub enum Json { Null, Bool(bool), Num(float), Str(str), Arr([Json]), Obj([str: Json]) }
pub native fn parse(text: str) -> Result<Json>
pub native fn stringify(value: Json, indent: int = 0) -> str
pub native fn encode<T>(value: T, indent: int = 0) -> str
pub native fn decode<reflect T>(text: str) -> Result<T>
pub native fn to_json<T>(value: T) -> Json
```

`Json` methods: `get(key: str) -> Json?`, `at(i: int) -> Json?`,
`as_str() -> str?`, `as_float() -> float?`, `as_int() -> int?` (integral
values only), `as_bool() -> bool?`, `as_list() -> [Json]?`,
`as_map() -> [str: Json]?`, `is_null() -> bool`, `path(p: str) -> Json?`
(dot-separated keys and indices: `"items.0.name"`).

JSON text embedded in source must be a raw string (`json.parse(r#"{"a": 1}"#)`),
since `{` starts an interpolation in ordinary strings (§2.5).

`stringify` emits compact JSON for `indent == 0`, otherwise pretty-prints with
`indent` spaces. Object keys keep insertion order. Non-finite numbers encode as
`null`. Integral floats within ±2^53 print without a fraction (`1.0` → `1`).

`encode`/`to_json` map values structurally: `int`/`float` → number, `str` →
string, `bool`, `()` → `null`, lists/sets/tuples → arrays, maps with `str` keys →
objects (other keys are converted with `to_str`), structs → objects keyed by
field name, `Option`: `None` → `null`, `Some(x)` → `x`; other enums: a
payload-less variant → `"Variant"`, otherwise `{"Variant": [payload...]}` (or an
object for named payloads); `bytes` → base64 string; `duration` → nanoseconds;
`Json` values as themselves.

`decode<T>` is the inverse for types built from those components: struct fields
missing from the object take their default value if one is declared and it is a
constant expression (a literal, `[]`, `[:]` or `None`), otherwise
`None` for option-typed fields, otherwise decoding fails with kind `"parse"`
and a message naming the path (e.g. `$.items[2].name: expected str`).
Parsing is limited to a nesting depth of 512.

### 13.9 `log`

`debug(msg: str)`, `info(msg: str)`, `warn(msg: str)`, `error(msg: str)`;
each accepts `fields: [str: str] = [:]`. `set_level(level: str)` with `"debug"`,
`"info"` (default), `"warn"`, `"error"`, `"off"`. Records go to the host log sink
(default: stderr as `LEVEL message key=value ...`). Capability `Stderr` with the
default sink.

### 13.10 `math`

Constants (`pub let` values): `pi`, `e`, `tau`, `inf`, `nan`. Functions on
`float`: `sqrt`, `cbrt`, `exp`, `ln`, `log2`, `log10`, `log(x, base)`, `sin`,
`cos`, `tan`, `asin`, `acos`, `atan`, `atan2(y, x)`, `sinh`, `cosh`, `tanh`,
`hypot(x, y)`, `floor`, `ceil`, `round`, `trunc`, `abs`, `pow(x, y)`,
`min(a, b)`, `max(a, b)`, `clamp(x, lo, hi)`, `to_radians`, `to_degrees`,
`is_close(a, b, rel_tol: float = 1e-9, abs_tol: float = 0.0) -> bool`.
Integer helpers: `gcd(a: int, b: int) -> int`, `lcm`, `isqrt(n: int) -> int`,
`abs_int`, `min_int`, `max_int`.

### 13.11 `net`

```
pub native fn listen(addr: str) -> Result<Listener>    // NetListen(addr)
pub native fn connect(addr: str, timeout: duration = 30s) -> Result<Conn>  // Net(host:port)
listener.accept() -> Result<Conn>
listener.addr() -> str
listener.close()
conn.read(max: int = 65536) -> Result<bytes>     // empty bytes at EOF
conn.read_line() -> Result<str?>                 // None at EOF; strips \n / \r\n
conn.write(data: bytes) -> Result<()>
conn.write_str(text: str) -> Result<()>
conn.peer_addr() -> str
conn.close()
```

Connections are made only to resolved addresses the policy allows (§14.6).
Reading and writing on the same connection from different tasks is allowed.
Listeners and connections count against `max_open_resources` until they are
closed (or collected), and so do sockets of operations still in progress:
connections being established, pending `accept` calls and HTTP requests in
flight; when the limit is reached, such operations fail with kind `"limit"`.
Once the peer has finished sending, `read` keeps returning empty bytes and
`read_line` keeps returning `None`.
TLS client connections: `net.connect_tls(host: str, port: int, timeout:
duration = 30s) -> Result<Conn>` (feature `tls`); `timeout` bounds
connecting and the TLS handshake together (kind `"timeout"`). The server certificate is verified for `host`
against the bundled web PKI roots plus any roots the host added
(`Config::tls_root_certificates`, §16); a verification or handshake failure
is an error (kind `"invalid"` for certificate problems), never a panic. The
same applies to `https` requests.

### 13.12 `random`

Engine-local PRNG (xoshiro256\*\*), seeded from host entropy unless the host
sets a seed. `int(lo: int, hi: int) -> int` (uniform in `lo..hi`; panics if
empty), `float() -> float` (`[0, 1)`), `bool() -> bool`,
`choice<T>(xs: [T]) -> T?`, `shuffle<T>(xs: [T])`, `sample<T>(xs: [T], k: int)
-> [T]`, `bytes(n: int) -> bytes`, `seed(s: int)`, `normal(mean: float = 0.0,
stdev: float = 1.0) -> float`. `random.Rng`: `random.rng(seed: int) -> Rng` with
the same methods (except `seed`), independent of the engine PRNG.

### 13.13 `regex`

```
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
Captures: get(i: int) -> Match?, name(n: str) -> Match?, len() -> int, text(i: int) -> str?
```

Syntax is that of the Rust `regex` crate (linear-time, no backreferences).
Write patterns as raw strings (`r"\d{4}-\d{2}"`): in ordinary strings `\d` is an
invalid escape and `{4}` is an interpolation (§2.5).
Compiled programs are limited in size (each automaton to a few MiB; larger
patterns, such as `\w{150}`, fail with kind `"limit"`). A compiled regex
counts against `max_memory` with the memory of its automata and of its search
cache, which grows as it is used (up to a few MiB), so a script cannot hold
more compiled regexes than the limit allows.

### 13.14 `statistics`

On `[float]`: `mean`, `median`, `variance` (sample), `stdev` (sample),
`pvariance`, `pstdev`, `min`, `max`, `sum`, `mode -> float?` (most common, first
on ties), `quantile(xs, q: float) -> float?` (linear interpolation, `0 <= q <=
1`), `correlation(xs, ys) -> float?`, `linear_regression(xs, ys) -> (float,
float)?` (slope, intercept). All except `sum` return `float?` and are `None` for
empty input (variance/stdev need at least two values). Also `mean_int([int])
-> float?`.

### 13.15 `test`

`assert_eq<T: Eq>(actual: T, expected: T, msg: str = "")`, `assert_ne`,
`assert_true(c: bool, msg: str = "")`, `assert_false`,
`assert_close(a: float, b: float, tol: float = 1e-9)`,
`assert_err<T, E>(r: Result<T, E>)`, `assert_ok`, `fail(msg: str) -> never`,
`property<T>(name: str, gen: fn(random.Rng) -> T, prop: fn(T) -> bool,
cases: int = 100, seed: int = 20240611)` (runs `prop` on `cases` values produced by
`gen` from a `random.Rng` seeded with `seed`; on failure panics with the
failing input's debug form). The default seed is fixed, so property tests are
deterministic and failures reproducible; pass a different `seed` (for example
one derived from `random.int`) to explore other inputs. Failure messages for `assert_eq` have the form
`assertion failed: left == right\n  left: <debug>\n right: <debug>`.

### 13.16 `time`

```
pub struct DateTime { unix_nanos: int }   // UTC instant
pub native fn now() -> DateTime
pub native fn monotonic() -> Instant
pub native fn parse(text: str) -> Result<DateTime>        // RFC 3339 / ISO 8601
pub native fn from_unix(secs: int) -> DateTime
pub native fn from_unix_millis(ms: int) -> DateTime
pub native fn date(year: int, month: int, day: int, hour: int = 0, minute: int = 0, second: int = 0) -> Result<DateTime>
```

`DateTime` methods: `year`, `month`, `day`, `hour`, `minute`, `second`,
`nanosecond`, `weekday() -> int` (Monday = 1 ... Sunday = 7), `day_of_year`,
`unix() -> int`, `unix_millis() -> int`, `add(d: duration) -> DateTime`,
`sub(d: duration) -> DateTime`, `since(other: DateTime) -> duration`,
`to_iso() -> str` (`2024-01-02T03:04:05Z`, with fractional seconds only if
non-zero), `format(fmt: str) -> str` (strftime subset: `%Y %m %d %H %M %S %f %j
%a %A %b %B %y %e %Z %z %s %%`, plus `%3f`/`%6f`/`%9f`). `Instant`: `elapsed() ->
duration`, `since(earlier: Instant) -> duration`. Also `time.sleep(d)` (same as
`task.sleep`).

### 13.16a `task`

See §11.3.

### 13.17 `uri`

```
pub struct Uri {
    scheme: str, userinfo: str?, host: str?, port: int?,
    path: str, query: str?, fragment: str?
}
pub native fn parse(text: str) -> Result<Uri>          // RFC 3986
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
- Paths are normalised lexically (`.`/`..` resolved) and, when they exist,
  canonicalised (symlinks resolved) before being checked, so a script cannot
  escape an allowed directory via `..` or symlinks. The checked location is
  then accessed without following symlinks (directories are opened one
  component at a time with no-follow semantics), so a symlink swapped into
  the path after the check cannot redirect the access (§13.3). On Windows,
  every component is opened relative to its parent directory's handle
  without following symlinks or junctions; `rename` is path-based there, but
  holds every directory on both paths open (so that none can be renamed or
  replaced) and checks that it is the directory the no-follow walk finds,
  leaving only a narrow race in which the parent directory of an entry is
  emptied and turned into a junction while the rename runs. On other
  platforms accesses are path-based and the race remains. On Windows, paths
  are compared case-insensitively and use the plain `C:\...` /
  `\\server\share\...` form (not `\\?\...`).

### 14.2 Capabilities

| Capability                  | Raised by                                  |
| --------------------------- | ------------------------------------------ |
| `FsRead(path)`              | reading files, listing dirs, metadata      |
| `FsWrite(path)`             | creating, writing, removing, renaming      |
| `Net(host, port)`           | outgoing TCP/HTTP/TLS connections          |
| `NetAddr(host, addr)`       | each address `host` resolved to (§14.6)    |
| `NetListen(addr)`           | listening sockets                          |
| `Env(name)`                 | environment variable reads (`"*"` for all) |
| `Stdin`, `Stdout`, `Stderr` | standard streams                           |
| `Import(path)`              | loading a script module                    |
| `Custom(name, detail)`      | host native modules may define their own   |

### 14.3 Decisions

A policy maps a request to `Allow`, `Deny` or `Prompt`. On `Prompt` the engine
calls the host's prompter, suspending **all** script execution until it
answers `AllowOnce`, `AllowAlways`, `DenyOnce` or `DenyAlways`. "Always" answers
are remembered for the identical request for the lifetime of the engine (across
all its runs and compilations); "once" answers apply to the single request
only: a library function that needs several capabilities (`fs.copy`) is
retried after each prompt, and the "once" answers given meanwhile apply to
that call alone, never to a later one. If no prompter is configured,
`Prompt` behaves as `Deny`.

`Import` requests are decided while the program is compiled (also by `check`
and `lint`), one per script module, depth-first in import order, with the
module's resolved absolute path. A module imported by several modules is
requested once.

### 14.4 Effects of a denial

- I/O functions return `Err` with kind `"permission_denied"`.
- `env.get` returns `None`.
- `print`, `eprint` and `io.write` panic with `permission denied: stdout` /
  `stderr`.
- A denied `Import` is a compile-time error. Compilation stops there, so no
  further imports are requested.

### 14.5 Built-in policies

- `Policy::deny_all()` — everything denied except `Stdout` and `Stderr`.
- `Policy::allow_all()` — everything allowed (for trusted scripts).
- `RulePolicy` — a rule list: allowed/denied read paths, write paths, network
  destinations, listen addresses, environment variable names, streams,
  imports, and a fallback decision for unmatched requests (`Deny` or
  `Prompt`). Deny rules take precedence over allow rules. Network patterns
  are `host`, `*.example.com` (strict subdomains), `*`, an IP address
  (`127.0.0.1`, `::1`) or a CIDR range (`10.0.0.0/8`, `fc00::/7`), each
  optionally followed by a port (`host:443`, `[::1]:8080`,
  `10.0.0.0/8:443`, `[fc00::/7]:443`). Host names are ASCII and compared
  case-insensitively, ignoring a trailing dot. IP and range patterns match IP
  literals used by scripts and resolved addresses (§14.6); IPv4-mapped IPv6
  addresses are compared as IPv4. IP and range deny patterns also match
  other spellings of the addresses they cover: the unspecified address
  (`0.0.0.0`, `::`, which connect to the local host) is matched as
  `127.0.0.1` and `::1`, and IPv6 addresses embedding an IPv4 address
  (IPv4-compatible, IPv4-translated, NAT64 `64:ff9b::/96`, 6to4
  `2002::/16`) also as that address. A malformed pattern matches nothing
  (`policy::is_valid_net_pattern` checks one; the CLI rejects them).
  `deny_private_networks()` additionally denies private and internal
  destinations (§14.6). A path deny rule also protects the directories
  containing it from being renamed, replaced or recursively removed as a
  whole (`Policy::denies_under`); custom policies are only asked about the
  top-level paths of such operations. Path rules are compared after
  normalisation (§14.1), which names existing files and directories with
  their stored spelling (on case- or normalisation-insensitive file systems
  `data/ſecrets` or `data/SECRETS` may name `data/secrets`). For operations
  on an entry itself (below), a final symlink is named with its stored
  spelling too, which is looked up in its directory; in a directory of more
  than 4096 entries it keeps the spelling it was given. Allow rules
  compare paths exactly (ignoring case on Windows). Deny rules, on every
  platform, compare paths caselessly and normalisation-insensitively
  (canonical decomposition, full case folding, default-ignorable characters
  removed), so a deny rule also covers other spellings of a path, including
  a not-yet-existing one (`data/ſecrets`, `café` with a precomposed or a
  combining accent). This only ever denies more.
- For operations on a directory entry itself (§13.3), the engine uses
  `Policy::check_entry`, which for a `RulePolicy` does not resolve a final
  symlink in the path; `Policy::check` always resolves it.

### 14.6 Network destinations

An outgoing connection (`net.connect`, `net.connect_tls`, every `http`
request and redirect hop) is checked in two stages:

1. `Net(host, port)` for the host name or IP literal the script used. This
   request may be prompted.
2. The name is resolved **once**. Each resolved address is checked as
   `NetAddr(host, addr)`; the connection is then attempted only to the
   allowed addresses, in resolver order, and the name is never resolved
   again. (An IP literal "resolves" to itself.) If every address is denied,
   the operation fails with kind `"permission_denied"` and a message naming
   the address. `NetAddr` requests are never prompted: a `Prompt` decision
   counts as `Allow`, since the user already decided the `Net` request.

`Policy::AllowAll` allows and `Policy::DenyAll` denies `NetAddr`; custom
policies are called with it (and can use `sandlang::policy::is_private_address`).
A `RulePolicy` denies a resolved address that matches an IP or range
`deny_net` pattern. With `deny_private_networks()` it also treats as
internal every address that is not public unicast — unspecified,
`0.0.0.0/8`, loopback, RFC 1918, shared/CGNAT `100.64.0.0/10`, link-local
(`169.254.0.0/16`, `fe80::/10`), unique-local `fc00::/7`, site-local,
multicast, broadcast and other reserved, documentation and benchmarking
ranges, with IPv6 forms embedding an IPv4 address (IPv4-mapped,
IPv4-compatible, NAT64 `64:ff9b::/96`, 6to4) classified by that address —
and then:

- an internal IP literal, or `localhost`, is allowed at stage 1 only by an
  explicit pattern (not `*` or `*.domain`) or a prompt answer; `localhost`
  must then resolve to loopback addresses;
- an internal address reached by resolving any other name is denied unless
  an IP or range `allow_net` pattern matches it.

Without `deny_private_networks()`, a `RulePolicy` allows every resolved
address not matched by a deny range. The `sand` command line enables it by
default (§17.5).

---

## 15. Resource limits

### 15.1 Limits

Configurable per engine:

| Limit                | Default    | Effect when exceeded                                                                                                     |
| -------------------- | ---------- | ------------------------------------------------------------------------------------------------------------------------ |
| `max_memory`         | 512 MiB    | run fails: `memory limit exceeded` (I/O whose buffers do not fit fails with kind `"limit"`)                              |
| `max_instructions`   | unlimited  | run fails: `instruction limit exceeded` (counted in fuel units, §15.2)                                                   |
| `max_cpu_time`       | unlimited  | run fails: `cpu time limit exceeded` (time spent executing, excluding waits and prompts)                                 |
| `max_wall_time`      | unlimited  | run fails: `time limit exceeded`; also bounds compilation, counted from its start                                        |
| `max_call_depth`     | 10,000     | panic: `stack overflow`                                                                                                  |
| `max_tasks`          | 10,000     | `spawn` panics: `task limit exceeded`                                                                                    |
| `max_open_resources` | 1,024      | open/connect/listen/accept and HTTP requests fail with kind `"limit"` (pending operations' sockets count, §13.11)          |
| `max_output_bytes`   | unlimited  | writes fail: `output limit exceeded`                                                                                     |
| `max_http_body`      | 64 MiB     | HTTP read fails with kind `"limit"`                                                                                      |
| `max_source_bytes`   | 16 MiB     | compile error: `source too large` (the main file and all imported scripts together; the standard library is not counted) |
| `max_compile_fuel`   | 10,000,000 | compile error: `program too complex to compile` (§15.3)                                                                  |

Memory is measured as the engine's accounting of live heap objects (including
their buffers and the copies of map and set keys held by their indexes) and
of the tasks themselves (call stacks, and the lists of tasks waiting for a
task or a channel), and is enforced after garbage collection. A call that
would grow a task's stack beyond the limit fails with the memory limit. It also includes the
buffers of pending I/O operations — file and socket reads, HTTP requests and
responses in transfer, standard input, and copies of data being written —
from when the operation starts until its result becomes a value. An
operation whose buffers would not fit fails with kind `"limit"` instead,
before allocating them, and the run continues (so a peer sending a huge
line or body cannot end a server script); files are checked by their size
before they are read, while network data is counted as it arrives. Open
connections also count their fixed buffers while they are open (from when
they are being connected or accepted until closed): 8 KiB each (16 KiB for
the connections of an HTTP server, 48 KiB for TLS connections, including an
estimate of the TLS session's buffers); opening a connection whose buffers
do not fit fails with kind `"limit"`, and an HTTP server answers such a
connection with `503 Service Unavailable` and closes it. In a session (§16,
§17.6), the instruction, CPU time, wall time and output limits apply to each
input; the memory limit applies to the session's heap, which persists. Library functions
also check the Rust-side scratch space they use while building a result
(output strings and lists, display and JSON text, sort buffers, hash keys)
against the limit before or while it grows, so a script cannot make the host
allocate much beyond `max_memory`. Garbage is collected before such a check
reports the limit. Limit violations are reported to the host as
distinct error variants and cannot be intercepted by scripts. The host may
also interrupt a running engine from another thread with an
`InterruptHandle`; this stops compilation, bytecode and long-running library
functions alike.

### 15.2 Fuel

Execution is measured in _fuel units_. The interpreter spends one unit per
function call, loop iteration and backward jump. Work done in one step by a
library function or a generic operation is charged in proportion to its size,
roughly one unit per simple loop iteration's worth of work (a few to a few
tens of nanoseconds):

| Work                                                                                        | Cost                        |
| ------------------------------------------------------------------------------------------- | --------------------------- |
| element visited, compared, hashed, copied or produced                                       | 1 unit                      |
| byte copied, compared or scanned (strings, bytes, output text)                              | 1/64 unit                   |
| byte processed one at a time (hashing, regex search, encodings, case conversion, JSON text) | 1/8 unit                    |
| compiling a regex                                                                           | 1,000 + 16 per pattern byte, + 1/8 unit per byte of compiled program |
| regex work (compiling, searching, iterating), at least                                      | 1 unit per microsecond it takes |

Elements include list, map and set elements, the components of values being
compared, hashed, displayed or encoded, JSON values, regex matches and sort
comparisons. Constant-time operations are free. For example, sorting a list of
n numbers costs about n·log₂(n) units, `==` on two lists of n integers about n
units, and `json.parse` about one unit per value plus one per 8 bytes of
text.

The work of a regex search is not always proportional to its text: with
leftmost-first semantics a search may look far past the match it returns
(iterating over the matches of `[a-z]*X|[a-z]` in a long run of letters scans
the rest of the text for every match), and a search the lazy DFA cannot handle
falls back to an engine whose cost per byte grows with the size of the
pattern. The engine does not report the bytes it examined, so a regex
function's charge is raised to one unit per microsecond of the time it has
taken (measured as it runs, and charged between matches when iterating).
Ordinary searches take a few to a hundred nanoseconds per unit of their
deterministic charge, so their fuel does not depend on timing; only work
that the per-byte cost underestimates by an order of magnitude or more is
charged by time.

Fuel counts against `max_instructions`. Long operations check the limits while
they run: once a scheduling slice's fuel is spent, they check the instruction
budget, the CPU and wall-clock limits and the interrupt flag at least every
20,000 units, so a single library call or comparison of huge values is
stopped promptly. The instruction limit is exact for library functions: a
call stops as soon as the fuel used by the run (including the bytecode
executed before the call) exceeds `max_instructions`, and so does bytecode
(within one unit). The generic operations of the interpreter (comparison,
display, concatenation, slicing) may overrun it by the bytecode fuel spent
since the last library call, less than 20,000 units. Operations that cannot
be interrupted internally (a single regex search) are charged before they
start, so they fail immediately when the remaining budget cannot cover
them. A task whose library calls use up its slice is preempted when the
call returns, letting other tasks run.

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
  `max_wall_time` has elapsed since it started, and with an interrupt error
  when the host interrupts the engine.
- **Local limits**, reported at the offending construct: syntactic nesting of
  expressions, types, patterns and blocks is limited to 256 levels (`nesting
  too deep`); strings nested inside string interpolations to 32 levels
  (`strings nested too deeply inside string interpolations`); types to 10,000
  components (`type too large`); the exhaustiveness check of one `match` to a
  fixed amount of work (`pattern too complex to check for exhaustiveness`);
  runtime type descriptions of `reflect` type arguments (e.g. the target
  type of `json.decode`). A description expands struct and enum definitions,
  describing each struct or enum instance (a definition with its type
  arguments) once and sharing it wherever it occurs, and each distinct type
  is described once per program, however many call sites use it. Building
  one description may take at most 100,000 units of work (about one per
  component built or type component examined), and all the descriptions of
  a compilation together at most 1,000,000 (`type too large to describe at
  runtime`); a polymorphically recursive type such as `struct P<T> { next:
  P<[T]>? }` cannot be described. Finally, the copies of deferred code
  (§7.1) in one function are limited to about a million instructions (`too
  much deferred code`).

---

## 16. Embedding API (Rust)

```rust
use sandlang::{Engine, Config, Limits, Policy, NativeModule, Value};

let mut module = NativeModule::new("geo");
module.function("dist", |a: f64, b: f64| -> f64 { (a * a + b * b).sqrt() });
module.async_function("lookup", |key: String| async move { Ok::<_, String>(key.len() as i64) });

let config = Config::new()
    .policy(Policy::deny_all())
    .limits(Limits { max_memory: Some(64 << 20), ..Limits::default() })
    .module(module);
let mut engine = Engine::new(config);
let outcome = engine.run_source("main.sand", r#"import geo; print(geo.dist(3.0, 4.0))"#).await?;
```

- `Engine: Send`. All script execution happens inside the future returned by
  `run_*`, on whichever thread polls it. Engines share no mutable state, so any
  number may run concurrently on different threads or tasks.
- `run_source(name, source)`, `run_file(path)`, `check_source`, `run_tests`.
- Captured streams: `Config::stdout(Box<dyn OutputSink>)`, `stderr`, `stdin`.
- `Config::tls_root_certificates(Vec<Vec<u8>>)` trusts additional
  DER-encoded root certificates for TLS (e.g. a private CA), in addition to
  the bundled web PKI roots; calls accumulate. An invalid certificate makes
  TLS connections fail with kind `"io"`. Without the `tls` feature it has no
  effect.
- Policies (§14): `Policy::check(&Capability)`, `Policy::check_entry` (for
  operations on a directory entry itself), `Policy::denies_under(path)`;
  `RulePolicy::{allow_net, deny_net, deny_private_networks, ...}`;
  `sandlang::policy::{is_private_address, normalize_path,
  normalize_entry_path}` for hosts writing their own policies or natives.
- `NativeModule::function` accepts closures whose argument and return types
  implement `FromValue`/`IntoValue`: `i64`, `f64`, `bool`, `String`, `Vec<u8>`
  (as `bytes`), `()`, `Vec<T>`, `Option<T>`, `Result<T, E: Display>` (as
  `Result<T, Error>`), `HashMap<String, T>` / `BTreeMap<String, T>` (as
  `[str: T]`), and tuples up to 4. The Sand signature is derived from the Rust
  types. `NativeModule::source` may add Sand declarations (types, pure-Sand
  helper functions, `native fn` with explicit signatures bound via
  `NativeModule::raw_function`).
- `Outcome { exit_code: i32 }`; errors are `Error::Compile(diagnostics)`,
  `Error::Runtime { message, trace }`, `Error::Limit(kind)`, `Error::Interrupted`.
- A Rust panic inside the engine (a bug in a library or host native, an I/O
  operation or a prompter) does not unwind into the host: it ends the run
  (or session input, or test) like a script panic, with `Error::Runtime`
  whose message starts with `internal error:`. This requires panics to
  unwind (the default `panic = "unwind"`). A session in which this happens
  is reset (below).

**Sessions.** `Engine::session()` returns a `Session`, which runs inputs one
at a time with the semantics of `sand repl` (§17.6), using the engine's
configuration (policy, prompter, limits, streams, host modules, module root
and working directory):

```rust
let mut session = Engine::new(config).session();
session.eval("var hits = 0\nfn hit() -> int { hits += 1\n hits }").await?;   // Ok(None)
assert_eq!(session.eval("hit() + hit()").await?, Some("3".to_string()));
```

- `eval(source) -> Result<Option<String>, Error>` checks, compiles and runs
  one input; `Some` holds the debug form of its value. After `exit(code)`,
  `eval` returns `Ok(None)` and `exit_code()` returns `Some(code)` (until the
  next input); the session remains usable.
- `type_of(expr) -> Result<String, Error>` checks an expression without
  running it or changing the session.
- `run_tests() -> Result<TestReport, Error>` runs the `test` blocks entered so
  far, against the current state.
- `reset()` forgets all inputs. `interrupt_handle()` interrupts the input being
  run (the engine's own handle does not affect sessions).
- `run_tasks_until(until, on_error) -> Option<T>` runs the tasks that earlier
  inputs left running until the future `until` completes, and returns its
  output; a front end calls it while waiting for the next input, so that
  tasks keep running between inputs (§17.6). The instruction, CPU time and
  output limits apply to each call as to an input; the wall-time limit does
  not apply. Permission prompts and reads of standard input wait until
  `until` has completed. If a task fails (panic, limit, interruption), all
  tasks are cancelled and the error is passed to `on_error` at once; the call
  then goes on waiting. If a task calls `exit`, it returns `None` at once.
  `cancel_tasks()` cancels the tasks left running.
- After an internal error (a caught Rust panic, see above) in `eval`,
  `run_tests`, `type_of` or `run_tasks_until`, the session's state may be
  inconsistent, so it is reset as by `reset()`: the error's message ends with
  `" (the session was reset)"` (`sandlang::session::RESET_NOTE`), and
  `was_reset()` returns true until the next call. "Always" answers given
  before are kept.
- `Session::is_complete(source)` tells whether a line-based front end should
  read more lines before evaluating (§17.6).
- `Session: Send`; its futures are `Send`. A session shares the "always"
  answers of its engine (§14.3). Dropping an `eval` future before it
  completes (e.g. on a host timeout) is like interrupting the input; if it
  was still loading imports, the input has no effect.

---

## 17. Tooling

### 17.1 `sand check FILE...`

Parses and type checks; prints diagnostics; exit code 1 on errors (2 if a file
cannot be read).

All subcommands exit with code 2, after a message on standard error, for usage
errors: an unknown subcommand or option, a missing file argument, or an option
value that cannot be parsed.

### 17.2 `sand lint FILE...`

Reports warnings (exit code 1 if any, unless `--allow-warnings`; 2 if a file
cannot be read or has errors):

| Lint                | Description                                                                      |
| ------------------- | -------------------------------------------------------------------------------- |
| `unused-variable`   | local binding or parameter never read (names starting with `_` are exempt)       |
| `unused-import`     | import never referenced                                                          |
| `unused-function`   | private function or method not reachable from the module's public items, top-level code or tests (see below) |
| `unnecessary-var`   | `var` never reassigned (use `let`)                                               |
| `unused-result`     | a `Result` value is discarded                                                    |
| `unreachable-code`  | statement after `return`, `break`, `continue`, `panic`, `exit`, or after a `loop` that no `break` leaves (labeled breaks from nested loops count) |
| `unused-label`      | loop label that no `break` or `continue` refers to                               |
| `naming`            | violates naming conventions (§2.4)                                               |
| `bool-comparison`   | comparing with `true`/`false` literal                                            |
| `self-assignment`   | `x = x`                                                                          |
| `len-zero`          | `x.len() == 0` (use `is_empty()`)                                                |
| `empty-block`       | empty `if`/`else`/loop body                                                      |
| `todo`              | call to `todo()`                                                                 |
| `interpolated-literal` | `{4}` in a string: a number literal interpolated without a format spec (usually meant as literal braces, §2.5) |
| `shadowed-variable` | `let`/`var` binding a name that is already bound, with a different type, by a binding of the same function that is still in scope (see below) |

`unused-function` applies to the module being linted. Its public items are
`pub` top-level functions and all methods of `pub` types; every other function
and method is private. A private function counts as used only if it is
reachable, through calls (including method calls and pipes), function values
and default arguments, from top-level code, a `test` block or a public item.
So a function referenced only by itself (recursion) or by other unused
functions is reported.

For `shadowed-variable`, the earlier binding may be a parameter, a `let`/`var`
(including destructuring), a loop variable, or a `match`/`if let`/`while let`/
lambda/comprehension binding, and must still be in scope at the new `let`:
bindings in sibling blocks (two branches of an `if`, two loops, two `match`
arms) do not shadow each other. A lambda belongs to its enclosing function (a
`let` in a lambda body may shadow a binding of the enclosing function), but
each named function and each `test` block is checked on its own, so a
function or test re-binding a top-level name is not reported.

Parameters of trait and impl methods are exempt from `unused-variable`, since
the trait dictates them; the names of impl methods are checked at the trait.

A lint can be suppressed for the next line with `// lint: allow(name)`, or for
a file with `//! lint: allow(name)`.

Linting checks the file first, with the default limits (§15.3). The lints
themselves are bounded like compilation: they may spend 64 units of fuel per
byte of source (at least 10,000,000), about one per syntax tree node and per
node examined again by a lint (for example a loop body searched for a
`break`); a file that needs more fails with `source too complex to lint`
(exit code 2).

### 17.3 `sand fmt FILE... [--check]`

Rewrites files in canonical style: 4-space indentation, one statement per line,
spaces around binary operators, no space before `(` in calls, `{` on the same
line, trailing commas in multi-line lists/arguments/fields, at most one blank
line between statements, at most two consecutive blank lines never, items
separated by exactly one blank line, line width 100 (calls, lists, maps and
parameter lists that do not fit are broken one element per line), comments
preserved. Formatting is idempotent: `fmt(fmt(x)) == fmt(x)`, and formatting
never changes the parse tree (modulo spans). With `--check`, files are not
written; the exit code is 1 if any file is not formatted. Exit code 2 if a file
cannot be read or parsed. Trait and impl bodies are laid out like struct
bodies: one method per line, with a blank line around methods that have a body.
Formatting is bounded like linting (§17.2): 64 units of fuel per byte of
source (at least 10,000,000), about one per syntax tree node and layout node
examined, per 64 bytes of text measured, and per byte of output (deeply
nested code is indented, so the output can be much larger than the source);
a file that needs more is not formatted: `source too complex to format`
(exit code 2).

### 17.4 `sand test FILE... [OPTIONS]`

Runs the module's top-level code with output captured, then every `test` block
as a separate task with a fresh view of top-level state _as initialised_. Each
test passes if it completes without panicking. Output: one line per test
(`ok name` / `FAIL name: message`) and a summary `N passed, M failed`. Exit
code 1 if any test failed, 2 for compile errors.

Tests are untrusted code like any script: `sand test` takes the policy and
limit options of `sand run` (§17.5, except `--` and script arguments) and,
like `run`, denies everything that is not allowed by them. A test that
exceeds a limit fails with the limit's message.

### 17.5 `sand run FILE [-- ARGS...]`

Options: `--allow-read=PATHS`, `--allow-write=PATHS`, `--allow-net=HOSTS`,
`--deny-net=HOSTS`, `--allow-private-net`, `--allow-listen=ADDRS`,
`--allow-env=NAMES`, `--allow-stdin`, `--allow-all`,
`--prompt` (prompt on the terminal for unmatched requests), `--max-memory=BYTES`
(suffixes K, M, G), `--max-time=DURATION`, `--max-cpu=DURATION`,
`--max-instructions=N`, `--max-compile-fuel=N`, `--seed=N`. Arguments after `--`, and positional
arguments after `FILE`, are passed to the script. Exit code: the script's exit
code (codes outside `0..=255` exit with 255), 1 for a runtime error or `?`
failure, 2 for compile errors, 3 for limit violations.

`--prompt` asks on the controlling terminal (`/dev/tty`; the console on
Windows), never on the script's standard streams, so piped input cannot answer
prompts. The answers are `y` (allow once), `a` (always), `n` or an empty line
(deny once) and `N` (never); end of input denies, and an unrecognised answer is
asked again up to three times before denying. Control characters in the
request are escaped, as are Unicode format characters (such as
bidirectional overrides), line and paragraph separators, spaces other than
U+0020 and combining marks. Before each question the terminal's rendition is
reset (a soft terminal reset and an SGR reset, on a fresh line) and a marker
line is shown, so output the script left concealed or styled cannot hide or
imitate the question. Without a terminal, prompted requests are denied.

Unless `--allow-private-net` (or `--allow-all`) is given, the policy denies
private and internal network destinations (§14.6): `--allow-net=*` does not
reach `localhost` or `10.0.0.5`, but `--allow-net=localhost:8080`,
`--allow-net=127.0.0.1` or `--allow-net=10.0.0.0/8` do. Malformed network
patterns are rejected.

### 17.6 `sand repl [OPTIONS]`

Interactive read-eval-print loop, taking the options of `sand run` (§17.5;
`--prompt` asks on the terminal as for `run`). It runs a session (§16): each
input is type checked in the context of the previous inputs, then compiled
and run **exactly once**; state persists between inputs.

- An input is any mix of what a module may contain (§3.1): statements,
  expressions and declarations, including `import`, `trait`, `impl` and
  `test` blocks. The input's names join the session's single module scope.
- If the input's last item is an expression statement, its value is printed
  in debug form (§10.9), unless it has type `()` or the input ends with `;`.
- Top-level `let`/`var` bindings are globals that later inputs may read; a
  `var` declared by an earlier input may be assigned. Within one input, names
  must be unique as in a module; a later input may define any name again (as
  any kind of item), which shadows the earlier definition for later inputs.
  Code checked earlier keeps referring to what it referred to then (a
  function calls the function it was checked against, not a later one of the
  same name). Methods (`extend`) and impls (§4.9) of a later input likewise
  replace those of earlier inputs with the same name or for the same type.
- A top-level variable's type must be inferred within the input that declares
  it.
- An input that fails to compile has no effect. An input that panics, exceeds
  a limit or is interrupted keeps its declarations and every effect that
  happened before the failure, including assignments to globals; globals it
  had not initialised yet remain uninitialised (reading them panics, §3.1). A
  top-level `?` that meets an `Err` aborts the input like a panic whose
  message is the error.
- Tasks spawned by an input keep running while later inputs run, and while
  the REPL waits for input, and can be awaited by later inputs. A panic in
  any task fails the input being run (or, between inputs, is reported as
  such). After a failed input or task, all tasks are cancelled. While the
  REPL waits for input, the tasks' output starts on a new line and the
  prompt is shown again after it; their permission prompts and reads of
  standard input wait until a line has been entered.
- `exit(code)` ends the session with exit code `code`.
- The instruction, CPU time, wall time and output limits apply to each input
  separately; the memory limit applies to the session's heap. Between
  inputs, the instruction, CPU time and output limits apply to the tasks'
  work during each wait for input; the wall-time limit does not apply then.
  "Always" answers to prompts apply to the rest of the session.
- After an internal error (a bug in the engine or in a host native, §16) the
  session is reset, as by `:reset`, and the error says so.
- `test` blocks are checked but not run when entered.

Input lines are read until the input is complete: while brackets, a string or
a block comment are open, or the last token is a binary operator, `=`, `,`,
`.`, `:`, `=>`, `->`, `else` or a similar token that cannot end a statement,
or the last line is a doc comment (`///`, which belongs to the next item), the
REPL prompts with `...` for more. Ctrl-C interrupts the input being run
(the session continues), discards a partially entered input, or, at an empty
prompt, cancels the tasks left running; end of input
(Ctrl-D) ends the session, running any partial input first. Results are
printed on standard output, diagnostics and errors on standard error. The
REPL's input is also the scripts' standard input (`--allow-stdin`); when it is
not a terminal, a script that reads it may consume input meant for the REPL.

Commands (on a line of their own):

| Command      | Effect                                                         |
| ------------ | -------------------------------------------------------------- |
| `:help`      | show the commands                                              |
| `:type EXPR` | show the type of an expression without running it (a generic function as `fn<T: Ord>([T]) -> T?`) |
| `:test`      | run the `test` blocks entered so far, against the current state (the last block of each name; effects persist), and print a report as `sand test` does |
| `:reset`     | forget all inputs and start a fresh session ("always" answers are kept) |
| `:quit`, `:q` | end the session (exit code 0)                                 |

---

## 18. Diagnostics

Diagnostics carry a severity (error/warning), a code, a message, and a primary
span (file, line, column) with optional notes. Rendered form:

```
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

The checker continues after errors where possible and reports up to 50 errors
per run. Parse errors stop checking of the affected module.
