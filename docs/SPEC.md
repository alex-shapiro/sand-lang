# The Sand Language Specification

Version 0.1

Sand is a sandboxed, statically typed scripting language. It is designed to be
embedded safely in a larger Rust process: all effects on the outside world are
mediated through a host-defined policy with authorization rules and resource limits.

## Contents

1. [Design goals](#1-design-goals)
2. [Source text and lexical structure](#2-source-text-and-lexical-structure)
3. [Programs, modules and imports](#3-programs-modules-and-imports)
4. [Declarations](#4-declarations)
5. [Types](#5-types)
6. [Expressions](#6-expressions)
7. [Statements](#7-statements)
8. [Pattern Matching](#8-pattern-matching)
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

## 1. Design goals

- Safety. It must be impossible for a script to affect state outside of the interpreter except where granted authorization by the host.
- Correctness. It should be simple to write code that behaves correctly and difficult or impossible to write code that behaves incorrectly.
- Inferred static typing. All Sand code is typed, and type inference is used wherever possible.
- Elegance and familiarity. Syntax must be concise and easy to understand for anyone who knows Rust, Python, or Swift.
- Performance. Sand should be as fast and memory-efficient as possible for a safe bytecode interpreter.
- Embeddable. A single process can embed multiple Sand interpreters and run them in parallel, safely.

## 2. Source text and lexical structure

### 2.1 Encoding

Source files are UTF-8. A leading byte-order mark is ignored. Source files use the
extension `.sand`. Non-ASCII characters may appear only in comments and string literals.

### 2.2 Comments

```sand
// line comment
/// documentation comment for the following item, field, variant or method
//! documentation comment for the enclosing module (only before the first item)
```

`help` displays doc comments (§12.4). A doc comment on an `extend` block
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

and the newline is not inside a `(...)` or `[...]` group. Inside `{...}`
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
`-` with whitespace before it and none after it (`-x`) is a negation:

- at the start of a line it begins a new statement, whatever the previous line ends with;
- after an operand on the same line, or the next line inside `(...)` or `[...]` it is an
  error (an ambiguous `-`): `a -b` and `[1` / `-2]` are rejected rather than read as `a - b` and `[1 - 2]`.

Every other `-` subtracts after an operand and negates elsewhere: `a - b`,
`a-b` and `a- b` subtract; `-a`, `f(-a)`, `[1, -2]`, `x = -1` and the second
`-` of `a - -b` negate. So a line starting with `-` and whitespace continues
the previous line:

```sand
let total = base
    - discount
```

The formatter prints subtraction with spaces on both sides and negation with none after.
Explicit `;` may separate statements on one line; redundant terminators are ignored.

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

Identifiers use convention casing, enforced by the linter. Types, traits, enum variants,
and type parameters are `UpperCamelCase`; functions, variables, fields, modules and loop
labels are `snake_case`; top-level `let` constants may also be `SCREAMING_SNAKE_CASE`.
The identifier `_` is a wildcard and may not be read.

### 2.5 Literals

**Integers**: `0`, `42`, `1_000_000`, `0xff`, `0b1010`, `0o777`. Underscores may
separate digits. A decimal literal must fit in a 64-bit signed integer.
A hexadecimal, binary or octal literal may be any 64-bit pattern, read as a
two's complement integer: `0x7FFF_FFFF_FFFF_FFFF` is the largest `int`,
`0x8000_0000_0000_0000` the smallest and `0xFFFF_FFFF_FFFF_FFFF` is `-1`.

**Floats**: `1.0`, `0.5`, `1e9`, `2.5e-3`, `1_000.5`, `inf`, `nan`.
A float must have digits on both sides of a `.`; `1.` and `.5` are not valid floats.

**Durations**: an integer or float literal immediately followed by a unit:
`ns`, `us`, `ms`, `s`, `m`, `h`, `d` (24 hours), `w` (7 days). Examples: `250ms`,
`1.5s`, `2h`, `3d`. The value is stored as a signed 64-bit number of nanoseconds.
Fractional nanoseconds are truncated.

**Booleans**: `true`, `false`.

**Strings**: `"..."`. Escapes: `\\`, `\"`, `\n`, `\r`, `\t`, `\0`, `\'`,
`\u{XXXX}` (1-6 hex digits, a Unicode scalar value), and `\(` which starts an
interpolation. Any other escape is an error, including `\{` and `\}`.
Strings may contain literal newlines; `{`, `}`, `(`, `)` and `:` are ordinary
text (`"[0-9]{4}"`, `"{\"a\": 1}"`).

**Interpolation**: inside a non-raw string, `\(expr)` inserts the display form
(§10.9) of `expr`; `\(expr:spec)` applies a format specification (§10.10). The
expression is lexed as ordinary code and may contain any tokens, including strings
with their own interpolations like `"\(xs.map(x => "<\(x)>"))"`. A line break is allowed inside
brackets opened within the interpolation; elsewhere, including in a spec, it is an
`unterminated interpolation` error, reported at the `\(`. `\()` is an `empty interpolation` error.

**Multi-line strings**: `"""` starts a string that ends at the next `"""`.
Escapes and interpolation work normally. A newline right after the opening `"""` is removed,
and the leading whitespace common to the non-blank lines and the closing `"""` line is removed
from every line. Lines that start inside an interpolation are code.

**Raw strings**: `r"..."`, `r#"..."#`, `r##"..."##` strings do not support escapes or interpolation.

**Byte strings**: `b"..."` produces a `bytes` value. Escapes work as in strings, plus `\xNN`.
Interpolation is not allowed. Non-ASCII characters are encoded as UTF-8.

### 2.6 Operators and punctuation

```text
+  -  *  /  %  **  &  |  ^  ~  <<  >>
== != <  <= >  >=  =  += -= *= /= %=
.. ..= => -> ?? ? . , : ; @ !
( ) [ ] { }
```

`!` occurs only in a macro call (§6.13). Negation is written `not`.

## 3. Programs, modules and imports

### 3.1 Modules

Each source file is a module. A module consists of an optional module doc
comment, `import` declarations, then a mix of item declarations (§4), `test` blocks,
and top-level statements.

When a module is executed, its top-level statements and top-level `let`/`var`
initializers run in source order. Functions, types and methods may be referred
to before their textual definition. A top-level variable read before its
initializer has run fails as `uninitialized`.

The **main** module is the module the host asks the engine to run. Each imported
module is executed exactly once, before the first module that imports it,
following a depth-first order of `import` declarations. Cyclical imports fail at
compile-time.

A REPL session (`sand repl`, §17.6) is implemented as a main module whose items arrive one at a time.
Each input is checked and run on arrival, and may shadow the names of earlier inputs.

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
`.sand` extension may be omitted, and other files cannot be imported. By default,
a module path must lie within the **module root**, the main module's directory.
A symlinked main file's imports and module root are those of its target. Each script
import is checked against the policy as `Import(path)` (§14).

A module name is resolved first to a host-registered native module, then to a stdlib module,
and then to `<library>/<name>.sand` if `Config::library` is set. The engine checks a library
module against the policy as `Import(path)`, and the library directory can be outside the module root.

Module items marked as `pub` are visible to importers; non-`pub` items are not.
Importing a non-exported item fails at compile time.

Imported items are qualified by their module name: `module.name` and `module.Type`.

### 3.3 Visibility

Any item declaration may be exposed publicly with a `pub` prefix: `pub fn`.
An item that lacks a `pub` prefix is considered private and cannot be referenced
outside of its module.

Value formatting and `json.encode` include all struct fields regardless of visibility.
A public enum's variants and a public trait's methods are always public. `impl` blocks
do not have visibility modifiers (§4.9). Importing modules can read a `pub var`, but only
the declaring module can set it.

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

Each function parameter must have a type annotation.
The function return type follows `->`; if omitted it is `()` (unit).

Parameters may have default values. Defaults are evaluated at each call that omits them,
in the callee's module scope. The default expression may refer to earlier parameters.
Parameters with defaults must follow all parameters without them.

The function body is a block whose value is returned. If a function returns `()`,
the final expression value is discarded.

Type parameters are listed in `<...>`. A `:` after a type parameter specifies
the traits (§4.9) that its type arguments must implement: `T: Ord + Describe`, `T: geo.Shape`, `T: Into<str>`.
A trailing `where` clause (`fn sum(self) -> T where T: Num`) also specifies trait bounds, and
can constrain the type parameters of an enclosing generic `extend` block,
struct, or enum for that method.

A type parameter marked `reflect` (`fn load<reflect T: Decode>(text: str) -> Result<[T]>`)
receives a runtime type description of its type argument. `T` may be forwarded to another
`reflect` type parameter, such as `json.decode<T>`. Each type parameter that appears
in a `reflect` type argument must itself be declared `reflect` on the enclosing function.
Local functions, trait methods, and `impl` methods cannot declare `reflect` type parameters.

If recursive calls nest constructed type descriptions more than 256 levels deep, execution aborts
with kind `"limit"`.

A function may be declared inside a block; such a function may capture enclosing variables and
and may call itself recursively. Local functions declared consecutively in the same block
may call each other. Their type parameters can only require traits that do not declare methods.

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

Structs are reference types; variable assignment shares a struct value without copying it.

Struct fields are separated by newlines or commas. A field is immutable unless
declared `var`. If a field defines a default and the type construction omits
that field, it is is evaluated with its default expression. A field default can
use traits without methods from the struct's type parameter bounds (`s: Set<T> = Set([])`
with `T: Hash`), but cannot use traits that declare methods (§4.9).

Fields and methods are private unless marked `pub` (§3.3).

A struct can declare a static method `init` that returns `Self` or `Result<Self, E>`.
If a struct declares `init`, `T(args)` calls `init` and returns its result.
Inside `init`, `T(args)` and `Self(args)` construct the struct field by field.
Native modules may declare `native fn init` on built-in or opaque types like
`Set([1, 2])` and `regex.Regex(pattern)`).

Enums and traits cannot declare `init`, and expressions cannot reference `T.init` as a member.
`init` must apply to all instances of a type: an `extend` block that declares `init` cannot
specialize the target's type parameters, and `init` cannot add trait bounds beyond those
declared on the type.

A struct declaration body may include methods. A method whose first parameter is `self` (no annotation)
is an instance method, called as `value.method(...)`. Any other method is static, called as `Type.method(...)`.

Within the struct declaration body, `Self` refers to the struct type (with its type parameters).

A struct may be generic: `struct Pair<A, B> { first: A, second: B }`.

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

Enum variants are separated by newlines or commas. A variant has zero or more
payload fields, which are all named or all positional.

Variants are referred to as `Enum.Variant` in expressions, e.g.
`Shape.Circle(1.0)` or `Shape.Circle(radius: 1.0)`. In a pattern, the enum
prefix is optional if the scrutinee type is known, and a sub-pattern can
match a named payload field by name: `Rect(h: height, ..)`.

`Option` and `Result` variants (`Some`, `None`, `Ok`, `Err`) are always in
scope, unqualified.

Enum values are immutable.

### 4.4 Extending types

```sand
extend Point {
    fn scale(self, k: float) -> Point { Point(x: self.x * k, y: self.y * k) }
}
```

An `extend` block adds methods to a struct or enum declared in the same module.
Types defined outside of the current module may be "extended" without an `extend`
block by implementing a trait (§4.9).

### 4.5 Type aliases

```sand
type Grid = [[bool]]
type Pair<T> = (T, T)
```

Type aliases are transparent: `Grid` and `[[bool]]` are considered the same type.
Aliases may not be recursive, and their type parameters must not require traits.

### 4.6 Top-level variables

```sand
let GREETING = "hi"
pub let LIMITS = [1, 2, 3]
var counter = 0
```

Top-level `let` and `var` declarations follow the same rules as local bindings.
Types must be fully inferred from their initializer and module-level uses.

### 4.7 Native declarations

The prelude, standard library, and native modules can declare `native`
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

A test block name is a string literal without interpolation. Test blocks are ignored
during normal runs and executed by the test runner (§17.4). Inside a test block, the `test`
module and its public items are imported automatically, at lower
precedence than module definitions and the prelude. Within a test, `?` returns `None`
and `Err` as test failures (§6.8).

### 4.9 Traits and impls

A **trait** defines a set of methods that types can implement. Generic code
can call the methods of the traits that its type parameters require.

```sand
/// Things with a textual description.
pub trait Describe {
    // required method
    fn describe(self) -> str

    /// A default method may call the trait's other methods.
    fn shout(self) -> str { self.describe().to_upper() }
}

pub trait Monoid {
    // static method: no `self`
    fn empty() -> Self
    fn combine(self, other: Self) -> Self
}

// trait requires supertraits
pub trait Shape: Describe + Eq {
    fn area(self) -> float
}

// trait with a type parameter
pub trait Into<T> {
    fn into(self) -> T
}

impl Describe for Point {
    fn describe(self) -> str { "(\(self.x), \(self.y))" }
}

// conditional (generic) impl
impl<T: Describe> Describe for [T] {
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

#### Trait declarations

A trait body may contain method declarations. Any trait method without a body is required;
any method with a body is a "default" method applied to any `impl` that does not override it.

Methods whose first parameter is `self` are instance methods; all other
methods are static methods. Trait methods share the visibility of the trait.

Within a trait declaration, `Self` represents the implementing type. Because
`Self` implements the trait and its supertraits, default methods can call any
method of the trait (`self.describe()`, `Self.empty()`) or of its supertraits.

A trait can declare type parameters (`trait Into<T>`) that do not have trait bounds.
Trait methods can declare their own type parameters with trait bounds, and `where`
clauses on a trait method can constrain only the method's own type parameters.

Default values on trait method parameters must be constants.

Supertraits follow a `:`: `trait Shape: Describe + Eq`. Any type parameter
bounded by `Shape` must also satisfy `Describe` and `Eq`. Supertrait hierarchies
must not contain cycles.

Traits share the type namespace. In a trait bound after `:`, a module-local
type does not shadow a prelude trait of the same name (`struct Hash` does not
shadow `T: Hash`). User code cannot redefine or implement built-in traits.

#### Trait Impls

An `impl` block implements a trait for a target type.

The target type must have a type constructor. Function types and bare type parameters
are not valid target types. Target type arguments can be concrete types like `impl Describe for [int]`.

Each type parameter of an `impl` block must appear in the target type or in trait arguments.
Trait bounds declared on the `impl` block must hold for every use of the `impl`.

An `impl` block must define every required method of the trait, may override
default methods, and must not declare methods that are not in the trait. Each
method signature must match the trait declaration after substituting the
target type for `Self` and the `impl` trait arguments for the trait's type
parameters. Method type parameters may be renamed if they declare equivalent
trait bounds.

Methods in an `impl` block cannot specify `pub`, parameter defaults, or bounds
on the `impl` block's type parameters. Within an `impl` block, `Self` is an alias
for the target type.

The target type must implement every supertrait of the implemented trait under the
`impl` block's trait bounds.

A trait must have at most one implementation for a given type constructor and
unifiable trait arguments. Two `impl` blocks for the same trait and type constructor
conflict if their trait arguments unify: `impl Into<str> for int` and `impl Into<float> for int`
do not conflict, but `impl Describe for [int]` and `impl Describe for [str]` do conflict.

**The Orphan rule:** an `impl` block must appear in the module that declares the
trait or the module that declares the target type.

Implementations are unnamed and always visible once their declaring module is loaded.

#### Trait bounds

A type argument passed to a parameter bounded by `T: Trait<Args>` must satisfy that bound.
A concrete type satisfies a bound through a matching `impl` if the type arguments satisfy
all bounds required by that `impl` recursively. For example, `[[Point]]: Describe` requires
`[Point]: Describe`, which requires `Point: Describe`. A type parameter satisfies its declared
bounds and their supertraits, and `Self` satisfies the enclosing trait and its supertraits. The
type checker verifies trait bounds at every instantiation and reports any unmet bound.
Example: ``str does not implement `Describe` (required for [str] to implement `Describe`)``).

Functions, methods, `where` clauses, `impl` blocks, structs, and enums may declare trait bounds
on their type parameters. Methods on a struct or enum, including methods in `extend` blocks,
inherit the type parameter bounds of the type. A struct or enum also infers any bounds that its
field or variant payload types require of its type parameters.

For example, given `struct Bag<T: Hash>`, `struct Outer<U> { b: Bag<U> }` infers `U: Hash`,
and `help(Outer)` displays `struct Outer<U: Hash>`. Because `Set` elements and map keys require
`Hash`, a field of type `Set<[int]>` is rejected at declaration time.

The type checker verifies type constructor bounds on constructions, enum variants, method calls,
struct fields, enum payloads, and `reflect` type arguments. Other type annotations are not checked.

A trait that does not declare methods in itself or its supertraits, for example `trait Marker {}`,
does not pass a runtime method dictionary to generic functions.

#### Trait method calls

For a call `e.m(args)`:

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
`Iterator<T>` (§12.3).

Standard library and host modules define additional nominal types. An _opaque_ type binds a
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

The prelude declares six built-in traits:

| Trait    | Supertraits    | Implemented by                                                                                                                                                                             |
| -------- | -------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `Eq`     |                | every type except function types, and types containing them                                                                                                                                |
| `Ord`    | `Eq`           | `int`, `float`, `bool`, `str`, `bytes`, `duration`, `()`, ordered value types (§5.4), and tuples, lists, `Option`, structs and enums all of whose components are `Ord`                     |
| `Hash`   | `Eq`           | `int`, `bool`, `str`, `bytes`, `duration`, `()`, `Range`, value types (§5.4), and tuples, `Option`, `Result`, enums and structs without `var` fields where all components implement `Hash` |
| `Encode` |                | every type except function types, `Task`, `Channel`, native objects (§5.4), and types containing them                                                                                      |
| `Decode` | `Encode`       | every `Encode` type except structs that declare `init` (§4.2) and types containing them                                                                                                    |
| `Num`    | `Ord + Encode` | `int`, `float`                                                                                                                                                                             |

The compiler automatically derives `Eq`, `Ord`, `Hash`, `Encode`, and `Decode` from a type's components.
A struct or enum implements a derived trait if every field and variant payload implements it, and a generic
type requires the trait on each type parameter used by those components.

Example: `struct Pair<A, B> { a: A, b: B }` implements `Hash` if `A` and `B` implement `Hash`,
and `help(Pair)` displays `Hash where A: Hash, B: Hash`.

Built-in trait conformance is a prerequisite for some operator use:

- `==` and `!=` require `Eq`
- `<`, `<=`, `>`, and `>=` require `Ord`

Built-in traits may be used as bounds on type parameters, supertraits, and `impl` blocks in the same way as user-defined traits (§4.9).

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

`[e; n]` evaluates `e` once per element. `n` must be a non-negative `int`.

Comprehensions may include multiple `for` clauses and `if` filters, evaluated left
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

#### Arithmetic

`+ - * / % **` and unary `-` require both operands to have the same `Num` type.
Additional allowed implementations:

- concatenation: `str + str`, `bytes + bytes`, `[T] + [T]`
- `duration + duration`, `duration - duration`, `-duration`
- `duration * k`, `k * duration` and `duration / k` produce `duration`,
  where `k: Num`. A `float` divisor gives the exact quotient truncated
  toward zero to whole nanoseconds. If the divisor is infinite, it returns `0s`.
  If the divisor is zero, it fails as `invalid`. If the divisor is `nan` or if
  the quotient is out of range, it fails as `overflow`.
- `duration / duration` produces `float`.

Integer semantics:

- Overflow in `+ - * **`, unary `-`, and `/` fails as `overflow`.
- `/` is floor division and `%` is floor modulo: `-7 / 2 == -4`, `-7 % 2 == 1`, `7 % -2 == -1`.
- Division or modulo by zero fails as `invalid`.
- `a ** b` with `b < 0` fails as `invalid`.

Float semantics: IEEE-754 with round-to-nearest.

- `/` by zero returns `inf`, `-inf` or `nan`.
- `%` is floor modulo (`a - b * floor(a / b)`).
- `**` is `powf`.

#### Bitwise

`& | ^ << >> ~` operate on `int`. `>>` is arithmetic. A shift
outside `0..64` fails with `invalid: shift out of range`.

#### Comparison

`== !=` require `Eq` operands of the same type and compare structurally.
`< <= > >=` require `Ord` operands of the same type.

Ordering rules:

- `bool`: `false < true`
- `int` and `float`: numeric; comparison with `nan` always returns false and `nan` is greater than every other float.
- `str` and `bytes`: lexicographic
- enums: variant declaration order, then payload
- structs: field by field in declaration order
- collections: compared by content
- opaque types: compared by identity

#### Logical

`and`, `or`, `not` take and return `bool`; `and`/`or` short-circuit.

#### Range

`a..b` (exclusive) and `a..=b` (inclusive) require `int` operands and produce a `Range`.

#### Coalesce

`a ?? b`: if `a: T?`, returns the payload of `Some` or else evaluates `b: T`.
If `a: Result<T, E>`, returns the `Ok` payload or else `b`.

### 6.6 Calls

```sand
f(1, 2)
f(1, scale: 2.0)              // named argument
point.norm()                  // method call
Point.origin()                // static method
Describe.describe(p)          // trait method in qualified form (§4.9)
http.get(url, timeout: 5s)    // module function with named argument
```

Positional arguments ordered first, then named arguments in any order.
Named and default arguments are available if the callee is known at compile time;
calls through function-typed values take positional arguments only.

### 6.7 Field access, indexing and slicing

- `s.f` reads struct field `f`; `t.0`, `t.1` read tuple elements.
- `xs[i]` indexes a list. An out of bounds error has kind `bounds`.
- `m[k]` looks up a map key. If the key is absent, it fails with kind `not_found`.
- `s[i]` on `str` and `bytes` is not permitted; use slices or methods.
- `xs[a..b]`, `xs[a..]`, `xs[..b]`, `xs[a..=b]` produce a new list.
  On `str` and `bytes`, slices produce new values and indices are byte offsets.
  Slicing fails if the range is out of bounds, if `a > b`, or if a range index is
  not on a character boundary.

### 6.8 Option and error propagation

Postfix `e?` where `e: T?` evaluates to the `Some` payload or returns `None`
from the enclosing function, whose return type must be an `Option`.

Postfix `e?` where `e: Result<T, E>` evaluates to the `Ok` payload or returns
`Err(err)` from the enclosing function, whose return type must be
`Result<_, E>` with the same `E`.

At the top level of the main module or in a test block, `e?` unwraps `Some`
and `Ok`. On `Err(err)`, `e?` fails execution with `err` (§10.3). On `None`,
`e?` fails execution with an error of kind `none`. Using `?` at the top level
of an imported module is an error.

`?` is not permitted inside a lambda unless the lambda's return type is an
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

A block's value is its final expression statement, or `()` if it ends with a
non-expression statement or is empty.

`if` without `else` has type `()`; its body's value is discarded. With `else`,
both branches must have the same type, unless the `if` is used as a statement.

`match` arms are separated by newlines or commas. Arms must be exhaustive
(§8.3). All arm bodies must have the same type unless the `match` is used as a
statement.

`loop` evaluates to the value given to `break` (or `()` if `break` has no
value). `while` and `for` evaluate to `()`; `break` in them takes no value.

`for` iterates over any value whose type implements the `Iterator<T>` trait (§12.3),
calling `next()` before each iteration until it returns `None`. A for loop binding
may be a pattern: `for (k, v) in map`.

A list loop visits indices `0..n`, where `n` is the length when the loop starts.
It visits each element once and stops early if the list shrinks during iteration.
Maps and sets are iterated over a snapshot taken when the loop starts.

`break` and `continue` refer to the innermost loop. `return e` returns from
the innermost function or lambda.

At the top level of the main module, `return` ends the program and may return a
value of any type; returning `Err(err)` fails with `err` (§10.3). At the top level
of an imported module it is an error. All three have type `never`.

#### Labels

A loop may be labeled by writing `@name` before `loop`, `while`
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

Loop labels must be declared locally: `break` and `continue` cannot exit a lambda or local function.
An unknown label, or one already used by an enclosing loop of the same function, is a compile error.

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

`"\(a) + \(b) = \(a + b)"` builds a string from the display forms (§10.9) of the embedded expressions.

### 6.12 `spawn`

`spawn e` evaluates expression `e` in a new task and returns a `Task<T>` where
`T` is the type of `e`. Variables are captured as for lambdas. See §11.

### 6.13 Macros

A macro call is written `name!(argument)`, always with parentheses.

The only macro today is `json!`, which builds a `json.Json` value from
JSON literals and Sand expressions:

```sand
let body = json!({
    "requests": [{"range": {"sheetId": id}, "fields": "bold"}],
    "values": rows,
    "note": null,
})
```

The argument is one of:

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

Rebinding a name with `let` in the same block shadows it.

Only `var` variables, `var` fields, list elements and map entries can be
assigned. Assigning a `let` binding or non-`var` field is a compile error.

`let ... else` requires the `else` block to have type `never`.

An expression statement whose value has type `Result<_, _>` and is unused
is reported by the linter (`unused-result`).

### 7.1 `defer`

`defer e` (usually `defer { ... }` or `defer x.close()`) schedules `e` to run
when the enclosing block exits, however it exits:

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

Deferred expressions in a block run when control leaves the block: by falling
off its end (after its value is computed), by `break` or `continue` to a loop
outside the block, by `return`, by `?` propagating an error or `None`, or through
the diverging `else` of `let ... else`. A `defer` statement that was not reached
does not run; a `defer` in a loop body runs at the end of each iteration that reached it.

The blocks of function bodies, lambdas, `spawn` expressions, `test` blocks
and the top level of a module are blocks too.

Top-level defers run when the module's top-level code finishes, or when a top-level
`?` or `return` stops the program.

Defers run in reverse order of declaration (last first); when an
exit leaves several blocks, the innermost block's defers run first.

A block's value, a `break` value and a `return` value are
computed first, then the defers run, then control leaves: `return f()`
calls `f()` before the defers, and a defer that assigns a variable does
not change a value already computed from it.

The deferred expression sees variables in scope where the `defer` statement
is declared and reads them when it runs. For example, `var s = 1; defer print(s); s = 2`
prints `2`.

A deferred expression must have type `()` and must not leave itself:
`return`, `?`, and `break`/`continue` to a loop outside it are compile
errors. Deferred code is compiled into every exit of its block, so a function with many
defers and exits may exceed a compile-time limit (§15.3).

An error (§10.3) or `exit` aborts the run without running defers.
A cancelled task (§11.2) runs its pending defers, innermost first;
cancelling it again has no effect. Deferred code may suspend.

### 7.2 `use`

A **resource** is a value whose type implements the prelude trait `Close` (§12.3).
Files and network connections are both resources. To automatically close a resource
when it leaves scope, bind it with `use`:

```sand
fn first_line(path: str) -> Result<str?> {
    use f = fs.open(path)?
    f.read_line() // f closes after this
}
```

`use name = e` (or `use name: T = e`) binds `e` immutably like `let` and
closes it as `defer Close.close(name)` would (§7.1): on every exit, last
declared first, interleaved with the block's defers. `use _ = e` keeps the
value only to close it (`use _ = lock.acquire()`).

A `use` binding is a name or `_` and is local to its block. If `use` is bound
at a module's top level it is closed when the top-level code finishes executing.
In a session (§17.6), a top-level `use` binds a session variable, as `let`.
Later inputs may read it and it is closed like a resource that is never closed.

If a `use` value is copied and referenced beyond the block close, operations fail
with kind `"closed"`. The linter reports escapes and unclosed resources.

Even without `use`, an unclosed resource may closed manually, or at latest is closed
automatically when it is unreferenced during garbage collection (§10.8).

## 8. Pattern Matching

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
scrutinee's enum type refers to that variant (`Empty` for a `Shape`, §4.3).

In a struct or variant pattern, positional sub-patterns match fields in declaration
order, followed by named sub-patterns. The pattern must either match every
field or end with `..`. Struct patterns match fields directly even if the
struct declares `init`. A pattern binds each name at most once.

A capitalized name that is not an enum variant is considered a binding. The linter reports
any capitalized binding as a potential typo. (§17.2).

### 8.2 Guards

`pattern if cond => body` matches only if `cond` is true. Guards do not count
towards exhaustiveness.

### 8.3 Exhaustiveness and reachability

A `match` must be exhaustive or else it fails as a compile error.

Types with infinitely many values require a wildcard or binding arm. An
arm covered by earlier arms is a compile error (`unreachable pattern`). A
`match`, or refutability check of a `let`, `for` or comprehension pattern,
whose check exceeds a fixed amount of work (e.g. or-patterns in many tuple
positions) is rejected with `pattern too complex to check for exhaustiveness`
(§15.3).

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

A run fails with an error if execution cannot continue. All tasks in the run end.
In a REPL session, the input ends with runtime error `<kind>: <message>`, the
display form of an `Error` (§5.4), and a stack trace of `file:line` entries.
Tools report it as `error: <kind>: <message>`, leaving out a kind of `error`.
A run fails with an error from:

- a top-level `?` or a top-level `return Err(...)`
- `fail(message, kind)` (§12.1)
- a fault detected during the run the script

  - `bounds` on an out-of-range index, slice or byte
  - `not_found` on a missing map key
  - `overflow` on integer or duration overflow
  - `invalid` on division by zero, a negative exponent, an argument out of its domain, a value JSON cannot encode
  - `limit` on stack overflow, nesting beyond 2,000 levels while printing, comparing or hashing a value, too many tasks
  - `unwrap` on `unwrap`/`expect` on `None`/`Err`
  - `assert` on a failed `assert`
  - `todo` on `todo()`
  - `cancelled` if waiting on a cancelled task
  - `deadlock` and `uninitialized` on a global used before initialization
  - `permission_denied` on sandbox permission denial

Scripts cannot catch faults, and failing does not run deferred expressions
(§7.1), except those of the top level for a top-level `?` or `return`; the
test runner isolates errors per test.

An **internal error** is a bug. It occurs if a Sand engine invariant is broken,
or else if a Rust panic was caught in the engine or in a host native.
Any internal error ends a run and is reported as `internal error: <message>`.

### 10.4 Exit

`exit(code: int) -> never` stops the engine run immediately with the given exit
code. It does not terminate the host process.

### 10.5 Functions and closures

Function calls are by value. Recursion depth is bounded by the `max_call_depth` limit,
with default 10,000. Exceeding recursion depth fails with `stack overflow`. Tail calls
do not consume stack depth, except while deferred expressions (§7.1) are pending,
since they run after the call returns.

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

Each Sand type can be formatted for display or debug. The display form is created with `.to_str()`.
The debug form is created with `.debug()`. Examples:

| Value                               | Display                             | Debug                                |
| ----------------------------------- | ----------------------------------- | ------------------------------------ |
| `42`                                | `42`                                | same                                 |
| `1.0`, `0.1`, `1e100`, `nan`, `inf` | `1.0`, `0.1`, `1e100`, `nan`, `inf` | same                                 |
| `true`                              | `true`                              | same                                 |
| `"hi"`                              | `hi`                                | `"hi"`                               |
| `b"a\x00"`                          | `b"a\x00"`                          | same                                 |
| `1500ms`                            | `1.5s`                              | same                                 |
| `()`                                | `()`                                | same                                 |
| `[1, 2]`                            | `[1, 2]`                            | same                                 |
| `["a": 1]`                          | `["a": 1]`                          | same                                 |
| `[:]`                               | `[:]`                               | same                                 |
| `Set` of 1, 2                       | `Set([1, 2])`                       | same                                 |
| `(1, "a")`                          | `(1, "a")`                          | same                                 |
| `Some(1)`                           | `Some(1)`                           | same                                 |
| `None`                              | `None`                              | same                                 |
| `Point(x: 1.0)`                     | `Point(x: 1.0)`                     | same                                 |
| `Circle(2.0)`                       | `Circle(2.0)`                       | same                                 |
| `Shape.Rect(w: 1.0, h: 2.0)`        | `Rect(w: 1.0, h: 2.0)`              | same                                 |
| `Error`                             | `<kind>: <message>`                 | `Error(message: "...", kind: "...")` |
| function                            | `<fn name>`                         | same                                 |
| task / channel                      | `<task>` / `<channel>`              | same                                 |
| `0..3`                              | `0..3`                              | same                                 |
| `time.PlainDate(2024, 2, 29)?`      | `2024-02-29`                        | same                                 |
| other opaque value                  | `<Regex>`                           | same                                 |

### 10.10 Interpolation Format Specifications

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

Examples:

- `"\(3.14159:.2)"` → `3.14`
- `"\(42:>5)"` → `   42`
- `"\(7:03)"` → `007`
- `"\(255:x)"` → `ff`
- `"\("ab":*^6)"` → `**ab**`
- `"\("hi":?)"` → `"hi"`.

## 11. Concurrency and asynchronous I/O

### 11.1 Model

Each engine runs one task at a time on one OS thread. All I/O is asynchronous:
when a task performs I/O it is suspended and other ready tasks run. There is no
`async`/`await` coloring — any function may suspend. Tasks are scheduled
cooperatively with preemption at instruction-count boundaries to prevent
a busy loop in one task from starving others.

When the main module's top-level code finishes, the run ends. Unfinished tasks
are cancelled, and their pending defers run until they finish or wait for I/O,
a timer or another task. If every task is blocked and no I/O or timer is pending,
the run fails with `deadlock: all tasks are blocked`.

### 11.2 Tasks

```sand
let t = spawn fetch(url)          // Task<Result<Response>>
let r = t.wait()                  // suspends until done
t.is_done()                       // bool
t.cancel()                        // cancels if still running
```

- `spawn e` creates a task that starts at the next scheduling point.
- `t.wait()` returns the task's result. Waiting on a cancelled task fails
  with `task cancelled`. Waiting on a task more than once returns the same
  value.
- Cancelling a task stops it at its next suspension point or instruction
  boundary and drops its pending I/O. Cancellation is immediate and runs
  all pending `defer` blocks (§7.1). A deferred expression interrupted by
  the cancellation does not resume.
- An error in any task aborts the run (§10.3).

### 11.3 The `task` module

The `task` module is always in scope without an `import` declaration. It
provides `task.sleep`, `task.yield_now`, `task.all`, `task.race`, and
`task.timeout` for coordinating tasks and timers.

### 11.4 Channels

`Channel(capacity: int = 0)` creates a FIFO message queue of type `Channel<T>`
for communicating between tasks. If `capacity` is `0`, the channel is
unbounded; if `capacity` is positive, `send` suspends the calling task while
the channel holds `capacity` queued messages; if `capacity` is negative,
`Channel` aborts with kind `"invalid"`.

`recv` suspends the calling task while the channel is empty and returns `None`
once the channel is closed and drained, whereas `try_recv` returns `None`
immediately if no message is queued. Closing a channel wakes any blocked tasks
so that subsequent `send` calls return `false` and receivers drain remaining
messages before receiving `None`.

### 11.5 Prompts suspend execution

All tasks pause while the engine waits on an interactive policy prompt (§14.3).
The decision must be applied before execution resumes.

## 12. The prelude

Prelude types, traits, functions, and the `task` module are in scope in every
module without an `import` declaration. The built-in types and traits declared
in the prelude are specified in §5; their methods are documented in the
standard library reference and via `help()` (§12.4).

### 12.1 Built-in functions

| Function                                              | Description                            |
| ----------------------------------------------------- | -------------------------------------- |
| `print<T>(x: T)`                                      | write display form + `"\n"` to stdout  |
| `eprint<T>(x: T)`                                     | write display form + `"\n"` to stderr  |
| `fail(message: str, kind: str = "error") -> never`    | abort execution with an error (§10.3)  |
| `assert(cond: bool, msg: str = "condition is false")` | abort with kind `"assert"` if `false`  |
| `todo() -> never`                                     | abort with kind `"todo"`               |
| `exit(code: int) -> never`                            | stop the run with an exit code (§10.4) |
| `help(x)`, `help()`                                   | print documentation (§12.4)            |
| `members(x) -> [str]`, `members() -> [str]`           | list member names (§12.4)              |

### 12.2 Universal methods

Every value supports `.to_str() -> str` (display form, §10.9) and
`.debug() -> str` (debug form, §10.9), unless shadowed by an inherent or trait
method (§9) such as `bytes.to_str() -> Result<str, Error>`.

### 12.3 Built-in protocol traits

The prelude declares two traits that integrate with language control flow and
resource management:

```sand
pub trait Iterator<T> {
    fn next(self) -> T?
    fn iter(self) -> Iter<T> { Iter(next: () => self.next()) }
}

pub trait Close {
    fn close(self)
}
```

- **`Iterator<T>` and `Iter<T>`:** `for` loops and comprehensions (§6.9) accept
  any value whose type implements `Iterator<T>`, calling `next()` before each
  step until it returns `None`. `Iter<T>` is a single-pass lazy iterator struct
  whose `next: fn() -> T?` closure produces elements on demand. When an
  `Iter<T>` pipeline is written as a single expression at the point where it is
  consumed (such as `for x in xs.iter().map(f).filter(g)` or
  `xs.iter().map(f).sum()`), the compiler fuses the pipeline into a single
  loop without intermediate closure allocations.
- **`Close`:** `use` bindings (§7.2) accept any value whose type implements
  `Close` and invoke `Close.close` when the enclosing block exits. The prelude
  implements `Close` for `T?` and `[T]` where `T: Close`. Closing an already
  closed resource is a no-op, and subsequent operations on a closed resource
  fail with kind `"closed"`.

### 12.4 `help` and `members`

The type checker resolves `help` and `members` at compile time to provide
interactive documentation and introspection:

- a module, whether imported or not: `help(json)`
- a type, trait, or type parameter: `help(json.Json)`
- a function, method, trait method, field, variant, or variable: `help(json.Json.get)`
- a string literal that identifies any of those targets: `help("json.Json.get")`
- any other expression, which is type-checked but not evaluated: `help([1, 2])`

`help(x)` writes the declaration, doc comment, and member summary of `x` to
standard output using the same `Stdout` capability and output limit rules as
`print`. `help()` prints an overview of the prelude and all registered modules.

`members(x)` returns the sorted list of member names accessible after `x.`: a
module's public items, or the variants, fields, and methods of a type or
expression. `members()` returns the sorted names in scope in the current
module, excluding prelude items.

Neither `help` nor `members` can be used as a first-class function value. A
module may declare its own `help` or `members` binding, shadowing the built-in
intrinsic.

## 13. Standard library

Standard library documentation is available in the standard library reference and through `help(<module>)` (§12.4).

## 14. Sandbox and policy

### 14.1 Principles

- Scripts have no access to the outside world except through standard library
  and host native functions.
- Every such access is described by a capability request and checked by the
  engine's policy before the effect occurs.
- The VM has no FFI, does not spawn processes, does not use IPC, shared memory
  or `mmap`, and never executes generated machine code.
- Paths are normalized lexically (`.`/`..` resolved). The longest existing
  prefix is then resolved like `realpath` (symlinks followed; `..` in a link
  target is the parent of the link directory). The engine checks the result so
  `..` and symlinks cannot escape an allowed directory. A symlink whose target
  cannot be resolved is not followed. Memory file system paths (§16.4)
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

| Capability                  | Raised by                                    |
| --------------------------- | -------------------------------------------- |
| `FsRead(path)`              | reading files, listing dirs, metadata        |
| `FsWrite(path)`             | creating, writing, removing, renaming        |
| `Net(host, port, tls)`      | outgoing TCP and TLS connections in `net`    |
| `Http(method, url)`         | each `http` request and redirect hop         |
| `NetAddr(host, addr, tls)`  | each resolved IP address for `host` (§14.6)  |
| `NetListen(ip:port)`        | listening sockets                            |
| `Env(name)`                 | environment variable reads, or `"*"` for all |
| `Stdin`, `Stdout`, `Stderr` | standard streams                             |
| `TimeZone`                  | `time.local_zone()`                          |
| `Import(path)`              | loading a script module                      |
| `Custom(name, detail)`      | host native modules                          |

### 14.3 Decisions

A policy evaluates each capability request to `Allow`, `Deny`, or `Prompt`.
If the policy returns `Prompt`, the engine suspends execution and calls the
host prompter for `AllowOnce`, `AllowAlways`, `DenyOnce`, or `DenyAlways`.
If the host does not configure a prompter, the engine denies the request.
An `AllowAlways` or `DenyAlways` decision persists for the lifetime of the
engine across all compilations and runs. An `AllowOnce` or `DenyOnce` decision
applies to a single request.

The compiler checks `Import` requests during `check`, `lint`, and compilation,
once per script module in depth-first import order, using the resolved absolute
path of the module. Importing a standard library or host module does not
generate an `Import` request.

### 14.4 Effects of a denial

- I/O functions return `Err` with kind `"permission_denied"`.
- A denied environment variable read returns `None`, or an empty map for `env.vars()`.
- `print` and `eprint` fail with kind `"permission_denied"` and message `permission denied: stdout` or `permission denied: stderr`.
- Writes to `io.stdout` and `io.stderr` return `Err` with kind `"permission_denied"`.
- A denied `Import` causes a compile-time error.

### 14.5 Rule policies

`RulePolicy` evaluates capability requests against specificity-ranked `allow`,
`deny`, and `prompt` rules. `Config::policy(RulePolicy)` sets the rule policy
for an engine and defaults to `RulePolicy::standard()`.
`Config::custom_policy(CustomPolicy)` replaces the rule policy with a
host-defined policy callback.

| Constructor               | Rules                                       |
| ------------------------- | ------------------------------------------- |
| `RulePolicy::default()`   | empty rule set                              |
| `RulePolicy::standard()`  | `allow stdout`, `allow stderr`              |
| `RulePolicy::allow_all()` | allows all capabilities for trusted scripts |

`RulePolicy` evaluates each request using the most specific matching rule. If
two rules have equal specificity, `deny` overrides `allow`, and `allow`
overrides `prompt`. If a request does not match any rule, the policy returns
`Prompt` (§14.3). `RulePolicy::extend` merges the rules of another
`RulePolicy`, and `Display` formats a `RulePolicy` as `.policy` text (§14.7).

`RulePolicy::with_defaults(defaults)` appends fallback rules that apply only if
the primary rules do not match a request. A host can use `with_defaults` to
combine user rules with host defaults (§17.5). Existing default rules on
`defaults` remain in the fallback tier. During stage-2 resolved-address checks
(§14.6), the policy compares the stage-1 allowing rule against IP address rules
by host specificity alone, so a user domain wildcard cannot bypass a default IP
deny rule.

Specificity depends on the capability category:

- **Files** (`FsRead`, `FsWrite`, `Import`):
  - A path pattern matches the target path and all subpaths, so `/data` matches `/data/a/b`.
  - Each segment can be a literal string, a `*` wildcard such as `*.pem`, a standalone `*` for a single segment, or a standalone `**` for zero or more segments. A pattern that starts with `**` matches across all roots, including all Windows drives; `**` alone matches every path.
  - A pattern with more non-`**` segments has higher specificity, so `deny ~/**/*.pem` overrides `allow ~/project` for `~/project/key.pem`. At equal segment depth, a pattern with fewer wildcards has higher specificity.
  - `FsWrite` access implies `FsRead` access. `FsRead` denial implies `FsWrite` and `Import` denial.

- **Network connections** (`Net`):
  - A host pattern matches `*` for all domains, an exact domain such as `example.com`, strict subdomains such as `*.example.com`, an IP address such as `127.0.0.1` or `::1`, or a CIDR prefix with zero host bits such as `10.0.0.0/8` or `fc00::/7`. Any host pattern can include a port, such as `host:443`, `[::1]:8080`, `10.0.0.0/8:443`, or `[fc00::/7]:443`.
  - A rule specifies `tls`, `plaintext`, or both transports. If a rule omits the transport, `allow` matches `tls` only, whereas `deny` and `prompt` match both `tls` and `plaintext`.
  - Specificity ranks exact hostnames and IP addresses first, then `*.localhost` per RFC 6761, then CIDR prefixes and domain wildcards by prefix length and domain depth, and finally `*`. The policy ranks an IPv4 `/N` prefix at `N + 96`, so `10.0.0.0/8` ranks at `/104`. A port increases specificity; transports do not affect specificity.
  - Hostnames must be ASCII and are compared case-insensitively without trailing dots. IPv4-mapped IPv6 addresses are evaluated as IPv4 addresses.
  - `allow` rules match only the literal address or hostname requested by the script. `deny` and `prompt` address rules also match equivalent unspecified addresses `0.0.0.0` and `::`, loopback addresses `127.0.0.1` and `::1` for unspecified addresses, and IPv4-embedded IPv6 addresses, including NAT64, 6to4, IPv4-translated, and IPv4-compatible forms. For example, `deny net 127.0.0.1` also blocks `0.0.0.0`, `::`, and `::ffff:0:7f00:1`.
  - Similarly, a `deny` or `prompt` rule for `localhost` also matches `*.localhost`, whereas `allow net localhost` matches `localhost` only. Hostname rules do not match IP literals; use `deny net localhost 127.0.0.0/8 ::1` to block both loopback names and loopback IP addresses.

- **HTTP requests** (`Http`):
  - `Http` rules use the same host, port, and transport matching as `Net` rules, and add optional HTTP methods and URL path patterns.
  - Specificity ranks the host first, then the URL path, and then the HTTP method.
  - The policy percent-decodes and lexically normalizes URL paths by removing `.` and `..` segments before comparison.
  - If a request path contains `\` or an encoded separator `%2F` or `%5C`, the policy evaluates the path both with and without treating those characters as path separators and applies the stricter decision. Policy patterns must use `/` as the separator.
  - An `allow net` rule for a destination permits raw TCP connections via `net` even if a `deny http` rule blocks HTTP requests to that destination.

- **Environment variables** (`Env`):
  - A pattern matches an exact name such as `AWS_SECRET_KEY` or a `*` wildcard such as `AWS_*`. `**` is invalid. Exact names have higher specificity than wildcards.
  - `env.vars()` requests `Env("*")`, which receives the strictest decision across all possible variable names: `Deny`, then `Prompt`, then `Allow`. An unmatched variable evaluates to `Prompt`, so `env.vars()` requires a catch-all rule such as `allow env *` to succeed without prompting.

- **Listening sockets** (`NetListen`):
  - Patterns are `ip:port`, `[ipv6]:port`, `ip`, `localhost` for any loopback address, `*:port`, or `*`.
  - Host `*` matches any bind address, port `*` matches any port, and port `0` matches only ephemeral port requests. IPv4-mapped IPv6 addresses are evaluated as IPv4 addresses.
  - Specificity ranks `ip` and `localhost` above `*`, and then ranks a specific port above `*`.

- **Custom capabilities** (`Custom`):
  - Native modules can define custom capabilities.
  - A rule matches a specific capability name such as `custom db` or all custom capabilities with `custom *`, and takes an optional detail such as `custom db drop` or `*`. An exact name has higher specificity than `*`, and an exact detail has higher specificity than `*` or an omitted detail. Quote literal asterisks, as in `custom db "a*b"`.

The policy normalizes file paths before comparison (§14.1) and resolves each
rule path on the first file request. `allow` rules compare path components
exactly, or case-insensitively on Windows. `deny` and `prompt` rules compare
path components case-insensitively and Unicode-normalization-insensitively on
all platforms by applying canonical decomposition, full case folding, and
default-ignorable code point removal. For example, `data/ſecrets`, precomposed
`café`, and decomposed `cafe\u{301}` all match `data/secrets` and `café`.

On case-insensitive file systems, the engine resolves existing paths to their
on-disk names; for example, `data/SECRETS` is resolved to `data/secrets`. For
directory entry operations (`fs.remove_file`, `fs.remove_dir`, and `fs.rename`), `RulePolicy::check_entry` evaluates the
on-disk name without following a symlink in the final path component. If the
operating system cannot read the on-disk name directly, such as for a symlink
outside macOS, a device node, or an unopenable file, the engine scans the
parent directory if it contains at most 4096 entries.

The engine blocks `rename` and recursive `remove_dir` on any directory tree that
contains an entry matched by a `deny` or `prompt` rule. The engine scans a
directory tree only if `RulePolicy::denies_under` indicates that a `deny` or
`prompt` rule can match a path inside it. For a `CustomPolicy`, the engine calls
`CustomPolicy::denies_under` and `CustomPolicy::protects` on the tree, which by
default protect any entry whose read or write access is denied.

### 14.6 Network destinations

The engine checks every outgoing network connection from `net.connect`,
`net.connect_tls`, `http` requests, and redirect hops in two stages:

1. **Host check:** The engine checks `Net(host, port, tls)` or
   `Http(method, url)` against the hostname or IP literal supplied by the
   script. The policy can prompt during this stage.
2. **Resolved-address check:** The engine resolves the hostname once and checks
   each resolved IP address as `NetAddr(host, addr, tls)`. An IP literal
   evaluates to itself, and `localhost` or `*.localhost` evaluates to
   `127.0.0.1` and `::1` without calling the system resolver per RFC 6761. The
   engine connects only to allowed addresses, in resolver order. If the policy
   denies all resolved addresses, the operation fails with kind
   `"permission_denied"` and reports the denied address and matching rule.
   `NetAddr` never prompts.

In stage 2, `RulePolicy::check_resolved` compares the host specificity of the
rule that allowed stage 1 against all IP address rules that match the resolved
address and transport, including `net` rules and `http` rules that match the
request method and path. An exact hostname has the same host specificity as a
single IP address, and `*.localhost` ranks above all CIDR prefixes. Wildcard
domain patterns such as `*.example.com` and `*` rank below all IP address rules.
If the stage-1 rule has higher specificity or if the IP address rules do not
match, the engine allows the address. Otherwise, the policy applies the most
specific matching IP address rule, and a `prompt` IP address rule denies the
connection because stage 2 cannot prompt. A `CustomPolicy` evaluates `NetAddr`
directly and treats `Prompt` as `Allow`.

This two-stage check prevents DNS rebinding against denied IP ranges. For
example, under `allow net plaintext *` and `deny net 10.0.0.0/8`, a request to
`http://evil.example/` fails if DNS returns `10.0.0.5`, whereas an explicit
hostname rule such as `allow net db.internal` outranks the `/8` prefix and
permits connecting to `10.0.0.5` via `db.internal`.
`sandlang::policy::is_private_address` reports whether an IP address belongs to
a private, loopback, link-local, or other non-public range.

### 14.7 Policy files

A policy file uses the `.policy` extension and declares authorization rules and
resource limits in a line-oriented text format. Pass a policy file to the CLI
with `sand run --policy=FILE` (§17.5), or parse it in Rust with
`sandlang::policy::PolicyFile::parse(text, base, home)` to obtain
`PolicyFile::rules` and `PolicyFile::limits`. A parsed `PolicyFile` contains
only the rules written in the file; a host can append default rules with
`file.rules.with_defaults(RulePolicy::standard())`.

Each line in a policy file is an independent statement. `#` starts a comment
that extends to the end of the line. Spaces or commas separate words. Double
quotes `"..."` support `\"` and `\\` escapes and quote literal characters so
that `*` is not treated as a wildcard. A word can combine quoted and unquoted
segments, as in `"/my data"/**`.

```policy
# Read the system and the project; keep secrets out.
let owners = alice bob
allow read /usr /System ~/.gitconfig
allow ./             # read and write the project
deny ./**/.env       # the more specific rule wins
prompt write ./dist  # ask before writing the build output
deny ~/Documents/*
allow http GET HEAD api.github.com
allow http POST PATCH api.github.com/repos/{owners}/**
allow net plaintext localhost:8080  # the development server, over http://
allow env HOME USER LC_*
limit memory 64M  # the prompter handles unmatched requests
```

| Statement                                                 | Meaning                                                                             |
| --------------------------------------------------------- | ----------------------------------------------------------------------------------- |
| `allow PATH...`, `deny PATH...`                           | all access to files (§14.5): `allow` reads and writes, and `deny` denies everything |
| `allow read PATH...`, `deny read PATH...`                 | `FsRead`; `deny` also denies writing and importing                                  |
| `allow write PATH...`, `deny write PATH...`               | `FsWrite`; `allow` also allows reading                                              |
| `allow import PATH...`, `deny import PATH...`             | script imports                                                                      |
| `allow net [TRANSPORT...] HOST...`, `deny net ...`        | connections and HTTP requests to hosts (§14.5)                                      |
| `allow http [TRANSPORT...] [METHOD...] URL...`, ...       | HTTP requests                                                                       |
| `allow listen ADDR...`, `deny listen ADDR...`             | listening addresses (§14.5)                                                         |
| `allow env NAME...`, `deny env NAME...`                   | environment variables, such as `AWS_*` or `*` for all                               |
| `allow stdin`, `deny stdin`, `deny stdout`, ...           | standard streams                                                                    |
| `allow timezone`, `deny timezone`                         | the local time zone                                                                 |
| `allow custom NAME [DETAIL]`, `deny custom NAME [DETAIL]` | a host-defined capability; `*` matches any name or detail                           |
| `prompt ...`                                              | same target syntax as `allow ...`, but prompts on match (§14.5)                     |
| `limit NAME VALUE`                                        | a resource limit (§15.1)                                                            |
| `let NAME = VALUE...`                                     | a value set; `{NAME}` in a rule expands to each value                               |

Every file path pattern must begin with one of the following prefixes:

- `/...`: root directory
- `./...`: directory containing the policy file, or the base path passed to the parser
- `../...`: parent of `./`
- `~/...`: user home directory
- `**/...`: any directory on every root

In `net` and `http` rules, optional case-insensitive transport keywords `tls`
and `plaintext` precede the host or URL patterns. If a rule omits the transport,
`allow` matches `tls` only, and `deny` or `prompt` matches both transports. The
parser rejects scheme and protocol names such as `http`, `https`, `ws`, `wss`,
`tcp`, `udp`, and `ssl` as transports. To match a hostname that collides with a
transport or HTTP method keyword, quote the hostname or append a trailing dot,
such as `"tls"`, `tls.`, or `delete.`.

In `http` rules, optional uppercase HTTP methods or case-insensitive standard
methods such as `GET` precede the URL patterns, before or after the transports.
If an `http` rule omits the method, the rule matches all HTTP methods. URL
patterns have the form `host[:port][/path]`, where `host` is a hostname, domain
wildcard, or single IP address, and `path` is a percent-decoded path pattern. A
URL pattern without a port matches all ports, and URL patterns must not include
a `scheme://` prefix. An `http` rule for an IP address also applies to stage-2
resolved-address checks for HTTP requests (§14.6).

A `let NAME = VALUE...` statement defines a reusable set of values anywhere in
the file. Referencing `{NAME}` inside a rule expands to each value in the set,
up to 100,000 expanded patterns per word or custom rule. Set values cannot
reference other sets.

`limit NAME VALUE` configures engine resource limits (§15.1):

- `memory`, `output`, `source`, and `http-body` accept byte counts such as `4096`, `64K`, `256M`, and `1G`.
- `time` and `cpu` accept durations such as `500ms`, `10s`, `1.5m`, `2h`, and `1d`.
- `instructions`, `compile-fuel`, `tasks`, `call-depth`, and `open-resources` accept integer counts such as `1_000_000`.
- `none` removes the limit for `memory`, `time`, `cpu`, `instructions`, `compile-fuel`, or `output`.

Unspecified limits retain their default values. The parser rejects any policy
file that contains a syntax or validation error and reports the 1-based line and
column on `PolicyError`.

`sand fmt`, `sand lint`, and `sand check` (§17) support `.policy` files. The
formatter normalizes whitespace, quotes special characters, preserves explicit
quotes, lowercases transports, uppercases HTTP methods, aligns trailing comments
on consecutive lines, and collapses multiple blank lines.
`sandlang::policy::lint_policy` checks a policy file for the following warnings
by comparing file paths lexically without resolving symlinks:

| Lint                   | Description                                                                                                  |
| ---------------------- | ------------------------------------------------------------------------------------------------------------ |
| `redundant-rule`       | broader rules or an earlier identical rule already produce the same decision                                 |
| `overridden-rule`      | higher-precedence rules produce another decision for all matching requests                                   |
| `bypassable-http-deny` | a `net` rule permits raw TCP connections to a destination that a `deny http` or `prompt http` rule restricts |
| `unused-set`           | a `let` set is not used in any rule                                                                          |

An `allow` rule for an exact hostname is not flagged as `redundant-rule` under a
domain wildcard if an IP address rule ranks between them, because the exact
hostname rule also overrides the IP address rule during stage 2 (§14.6).

`sandlang::policy::rule_for(capability, allow)` formats a persistent `allow` or
`deny` rule line for a promptable `Capability`, such as `allow read /data/a.txt`
or `deny http tls GET example.com:443/a`.

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
| `max_open_resources` | 1,024      | opening files, connect/listen/accept and HTTP requests fail with kind `"limit"` (after a collection, §10.8; pending operations' sockets count)                 |
| `max_output_bytes`   | unlimited  | writes fail: `output limit exceeded`                                                                                                                           |
| `max_http_body`      | 64 MiB     | HTTP read fails with kind `"limit"`                                                                                                                            |
| `max_source_bytes`   | 16 MiB     | compile error: `source too large` (the main file and all imported scripts together; the standard library is not counted)                                       |
| `max_compile_fuel`   | 10,000,000 | compile error: `program too complex to compile` (§15.3)                                                                                                        |

Memory is the engine's accounting, enforced after garbage collection, of
live heap objects (with their buffers and map and set indexes), freed heap
slots that have not been reused or released, garbage collector tables, tasks
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

`max_memory` does not include system allocator metadata or freed blocks that
the allocator retains. This allocator overhead is typically under 25% of
`max_memory`, but can be higher if a script frees many allocations of one
size class and then allocates another size class.

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

## 16. Embedding API (Rust)

The `sandlang` crate embeds the Sand compiler and virtual machine in a Rust host
application. Each `Engine` encapsulates an isolated execution environment
configured through `Config` with a sandbox policy (§14), resource `Limits`
(§15), host I/O providers, and optional `NativeModule` definitions. Engines do
not share mutable state and `Engine` implements `Send`, so a host can run
multiple engines concurrently across threads or asynchronous tasks, or
interleaved on a single thread in WebAssembly (§16.5).

```rust
use sandlang::{Config, Engine, Limits, NativeModule, RulePolicy};

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
that the compiler denies any import that requires an interactive permission
prompt.

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

If `sandlang` is compiled with the default `panic = "unwind"` setting, the
engine catches Rust panics inside library native functions, host callbacks, I/O
operations, and permission prompters and converts them into `Error::Internal`
so that panics do not unwind into the host.

### 16.2 Host environment and providers

`Config` allows the host to virtualize every external input, output, and source
of nondeterminism—streams, logging, process metadata, randomness, time, file
systems (§16.4), and networking—so that script execution can be made completely
deterministic. By default, `Config::new()` uses `RulePolicy::standard()`, which
allows only `Stdout` and `Stderr`, without a prompter. As a result, the engine
denies all other capability requests, including script imports unless a rule
such as `allow import ./lib` permits them. It writes output to the process
standard streams and provides an empty standard input.

The host can capture standard output and standard error or feed standard input
into a running script. `Config::stdout` and `Config::stderr` redirect script
output to an `OutputSink`, while `Config::stdin_bytes`, `Config::inherit_stdin`
with feature `os`, and `Config::stdin_feed` configure standard input. With
`StdinFeed`, scripts read chunks pushed via `StdinFeed::send`, where
`StdinFeed::end` terminates a single read, and `StdinFeed::wanted` is resolved
if a script is waiting for input and all other tasks are blocked.

The host can intercept and filter structured log records instead of
writing them to standard error. `Config::log_sink` routes records to a
`LogSink` as `LogRecord { level, message, fields, location }` on the polling
thread, using `LogSink::enabled` to filter records by level before allocation.
`Config::log_level` sets the initial minimum script log level and defaults to
`LogLevel::Info`, or disables script logging if set to `None`.

Command-line arguments, environment variables, and random number generation can
be fixed per engine. `Config::args` and `Config::env` replace the process
arguments and environment variables exposed by `env`, and `Config::seed` seeds
the pseudorandom number generator used by `random` and `uuid`.

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
bundled Web PKI roots; an invalid certificate causes TLS handshakes to fail
with error kind `"io"`.

Module search paths and capability policies are configured on `Config` before
constructing an `Engine`. `Config::module_root`, `Config::cwd`, and
`Config::library` set the directories used for relative file paths and `import`
resolution (§3.2), where registered standard and host modules from
`Engine::module_names` take precedence over library scripts. `Config::policy`,
`Config::custom_policy`, and `Config::prompter` set the policy and the
interactive `Prompter` (§14).

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
| `bind::<T>()`                | Rust type `T` with a derived `NamedType`                                            |
| `params(names)`, `doc(text)` | parameter names and documentation for the most recently registered function         |

Four derive macros bind a Rust type to a Sand type in a native module and
implement `NamedType`, `SandType`, `FromValue`, and `IntoValue`. Every derived
type requires `#[sand(module = "<module>")]` and accepts `#[sand(name = "<Name>")]`
to override its Sand name, `#[sand(private)]` to keep the type private to its
module, and `#[sand(crate = <path>)]` to specify a renamed `sandlang`
dependency. `SandEnum` and `SandStruct` also accept
`#[sand(manual_conversion)]` to omit the derived `FromValue` and `IntoValue`
implementations. The macros copy Rust doc comments, including
`#[doc = include_str!("...")]`, into Sand documentation, and reject type,
variant, or field names that are Sand keywords or invalid Sand identifiers.

| Derive       | Sand type                                                                  |
| ------------ | -------------------------------------------------------------------------- |
| `SandValue`  | opaque value type (§5.4); the type must also implement `ValueType`         |
| `SandEnum`   | `enum` with the variants and payloads of a Rust enum                       |
| `SandObject` | opaque reference type; the type must also implement `NativeObject`         |
| `SandStruct` | `struct` with the named fields of a Rust struct, copied on each conversion |

Each field of a `SandStruct` and each payload of a `SandEnum` variant must
implement `IntoValue` and owned `FromValue`. Unit variants map to unit
variants, tuple variants map to variants with positional payloads such as
`Circle(float)`, and struct variants map to variants with named payloads such
as `Rect(w: float, h: float)`, where `#[sand(name = "<name>")]` overrides a
payload field name. Generic type parameters on a `SandEnum` or `SandStruct` map
to Sand type parameters; pass any concrete instantiation to `bind`, such as
`bind::<Pair<(), ()>>()`.

By default, struct fields are `pub` and immutable; `#[sand(private)]` makes a
field private to the module, `#[sand(var)]` makes it mutable with `var`, and
`#[sand(name = "<name>")]` overrides its Sand name. `#[sand(default)]` uses
`Default::default()` as the Sand field default, and `#[sand(default = <expr>)]`
uses `<expr>`. `bind` panics if a field default is not a constant built from
numbers, `bool`, `()`, strings, `bytes`, durations, tuples, `Vec`, maps, sets,
`Option`, `SandEnum`, and `SandStruct` values, with at most 65,536 nodes and 64
nesting levels.

Recursive types can reference themselves through `Vec`, map, or `Box` fields.
Because the engine copies each reachable struct and enum value during
conversion and charges fuel and memory for each copy, shared references in a
Sand value produce independent Rust copies. The engine aborts conversion with
kind `"limit"` if struct and enum payload nesting exceeds 256 levels, such as
in a cyclic value, or if a `json.Json` value exceeds 512 nested arrays and
objects. Module Sand source can define methods on any bound type with `extend`.

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
  aborts the run with a runtime error via `Ctl::fail`, a resource limit error,
  or an exit code.
- `Result<Later<T, E>, E>` suspends the calling task on a future and maps to
  the same Sand return type as `Result<T, E>`.

To check a sandbox capability inside a `vm_function`, call
`Vm::require(&capability)` (§14), which returns an error of kind
`"permission_denied"` if the policy denies the request. Because the engine
re-runs a `vm_function` from the start after a permission prompt or after a
garbage collection triggered by `Vm::check_memory`, `Vm::reserve_io`, or
`Vm::resource_slot`, the function must check capabilities and limits before
performing any side effects. `Config::retry_stress` tests this requirement by
failing the first memory or resource check in each `vm_function` call.
Return-value conversion does not fail on the memory limit; instead, the engine
collects garbage and checks the memory limit after the call completes.

| Rust type                                           | Sand type                                              |
| --------------------------------------------------- | ------------------------------------------------------ |
| `i64`, `f64`, `bool`, `()`, `jiff::SignedDuration`  | `int`, `float`, `bool`, `()`, `duration`               |
| `String`, `&str`; `Vec<u8>`, `&[u8]`                | `str`; `bytes`                                         |
| `Vec<T>`, `Option<T>`, tuples of 2 to 8 elements    | `[T]`, `T?`, tuples                                    |
| `Box<T>`                                            | `T`                                                    |
| `HashMap<K, V>`, `BTreeMap<K, V>`, `IndexMap<K, V>` | `[K: V]`                                               |
| `HashSet<T>`, `BTreeSet<T>`                         | `Set<T>`                                               |
| `Result<T, E>`                                      | `Result<T, Error>` or `T`, depending on `E`            |
| `Never`                                             | `never`                                                |
| `sandlang::json::Json`                              | `json.Json`                                            |
| bound type `T`                                      | `T`, or `&T` for a borrowed object parameter           |
| `Val<T>`                                            | `T`, passed as an uncopied heap handle                 |
| `Param<N, B>`, `Reflect<N>`                         | `N`-th type parameter and its runtime type description |
| `Num<N>`                                            | `Num` evidence of the `N`-th type parameter            |

`Param<N, B>` specifies the built-in traits `B` required on the `N`-th type
parameter: `traits::Eq`, `traits::Ord`, `traits::Hash`, `traits::Num`,
`traits::Encode`, or a tuple such as `(traits::Ord, traits::Hash)`, defaulting
to `()`. The `native fn` declaration must declare these traits for that type
parameter, directly or through traits that have them as supertraits. Native
functions can only require built-in traits.

If a `native fn` declaration requires `Num` of its `N`-th type parameter, the
Rust function can accept a `Num<N>` argument providing `Num::zero` and
`Num::one` after any `Reflect` arguments and in type-parameter order. Native
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
`Config`, sharing the compiled modules across cloned configurations, and
returns `Error::Compile` if:

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

- `os`, enabled by default: enables access to the host file system, network
  sockets in `net` and `http`, process standard input via
  `Config::inherit_stdin`, and process environment variables if `Config::env`
  is not set. If `sandlang` is built without `os`, the engine cannot access the
  operating system regardless of the configured policy: `net` and `http` are
  unavailable, the environment is empty unless set via `Config::env`, standard
  input comes only from `Config::stdin_bytes` or `Config::stdin_feed`, scripts
  use an in-memory file system, and the crate compiles for
  `wasm32-unknown-unknown` (§16.5).
- `tls`, enabled by default and implying `os`: enables TLS support for `https`
  and `net.connect_tls` using `rustls` and `ring`, which requires a C compiler
  for the target. Build with `default-features = false, features = ["os"]` to
  disable TLS while keeping OS access.

`Config::memory_fs` replaces host disk access with an in-memory `MemFs` for the
`fs` module, script imports, and `Engine::run_file`. Without the `os` feature,
each engine uses an empty `MemFs` unless `Config::memory_fs` provides a
pre-populated instance. Clones of a `MemFs` share the same underlying files, so
the host can populate input files before a run, inspect output files afterward,
or share a virtual file system across engines using `write`, `read`,
`create_dir_all`, `remove`, `clear`, and `files`. `MemFs::with_capacity`
bounds total storage and defaults to 64 MiB, counting 64 bytes of overhead per
entry in addition to its name and contents. Working directory and module root
paths are resolved inside the `MemFs` with default `/`, and `RulePolicy`
evaluates `MemFs` paths against lexically normalized policy rules on all
platforms.

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
run on the WebAssembly linear memory stack, which is sized for the maximum
parser nesting depth. The engine executes code synchronously on the calling
thread and yields to the JavaScript event loop only while waiting for a timer,
an asynchronous I/O operation, or a permission prompt; hosts should run engines
inside Web Workers to avoid blocking the UI thread and to run multiple engines
in parallel.

The `sandlang-wasm` crate in `wasm/` exposes this build as a JavaScript `Engine`
class configured with a policy file, initial virtual files, environment
variables, arguments, standard input, a time zone, and `onOutput` and
`onPrompt` callbacks. The engine denies any prompt if `onPrompt` is not
configured. The library provides `run`, `check`, `lint`, `format`, and
`highlight` methods for execution and static analysis tasks.

### 16.6 Sessions

`Engine::session()` creates a stateful `Session` that implements `Send`,
returns `Send` futures, evaluates inputs incrementally under the engine's
configuration, and shares the engine's remembered `"always"` prompt decisions
(§14.3). `Session` implements the evaluation semantics of `sand repl` (§17.6):

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
inputs until the future `until` completes, returning the output of `until`, or
`None` immediately if a task calls `exit`. Instruction, CPU-time, and output
limits apply to each call, whereas `max_wall_time` does not, and permission
prompts wait until `until` completes. If a background task fails with an error,
limit, or interruption, the session cancels all running tasks, passes the error
to `on_error`, and continues waiting for `until`. `cancel_tasks()` cancels
remaining background tasks without running their `defer` blocks.

`Session::set_wall_time` overrides `max_wall_time` for subsequent inputs, and
`session.interrupt_handle()` cancels the active session call; the parent
engine's interrupt handle does not affect sessions. Dropping an `eval` future
before completion cancels the input in the same way as an interrupt, and leaves
the session state unchanged if the input was still loading imports. `reset()`
clears all session bindings and state. If an internal error (§10.3) occurs
during `eval`, `run_tests`, `type_of`, or `run_tasks_until`, the session
automatically resets itself, appends `" (the session was reset)"` from
`sandlang::session::RESET_NOTE` to the error message, and sets `was_reset()`
to `true` until the next call while preserving remembered `"always"` prompt
decisions.

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
default arguments, or `help` and `members` arguments, including functions
referenced only by themselves or by other unused functions.

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
code 1 if any test failed, 2 for compile errors. `sand test` accepts the policy
and limit options of `sand run` (§17.5) and prompts for any capability request
that the configured rules do not match. A test that exceeds a resource limit
fails with that limit's message.

### 17.5 `sand run FILE [-- ARGS...]`

`sand run` executes `FILE` under a `RulePolicy` combined with the CLI default
rules `allow stdout`, `allow stderr`, `allow stdin`, `allow timezone`, and
`allow import **` via `RulePolicy::with_defaults` (§14.5). Default rules apply
only if user rules from CLI flags or `--policy=FILE` do not match a request,
so `--prompt=stdin` prompts before the script reads standard input. For any
request that evaluates to `Prompt`, the CLI prompts interactively on the
controlling terminal.

Rule options `--allow=RULE`, `--deny=RULE`, and `--prompt=RULE` add rules
using the `.policy` syntax without the leading verb (§14.7), such as
`--allow='read ./data'`, `--allow='net plaintext localhost:8080'`, or
`--deny=stdin`. Relative paths are resolved against the current working
directory. `--policy=FILE` loads rules and limits from a `.policy` file; CLI
rule flags merge into the same primary rule tier, where `deny` overrides `allow`
at equal specificity, and CLI limit flags override limits from the file.
`--allow-all` replaces the CLI default tier with `RulePolicy::allow_all()`, so
explicit `--deny` or `--prompt` flags still take precedence; for example,
`--allow-all --prompt='write **'` prompts before every file write. `--no-prompt`
disables interactive prompts and denies any request that evaluates to `Prompt`.

Limit and execution options: `--max-memory=BYTES` with suffixes `K`, `M`, and `G`,
`--max-time=DURATION`, `--max-cpu=DURATION`, `--max-instructions=N`,
`--max-compile-fuel=N`, and `--seed=N` for the PRNG seed. Options that take a
value accept either `--opt=VALUE` or `--opt VALUE`.

Arguments after `--` and positional arguments after `FILE` are passed to the
script in `env.args()`. Exit codes: the script's exit code, clamped to 255 if
outside `0..=255`, `1` for an uncaught runtime error or `?` failure, `2` for a
compile or usage error, and `3` for a resource limit error. If the policy
allows `TimeZone`, `time.local_zone()` returns the host system time zone.

Interactive prompts read and write directly on the controlling terminal
—`/dev/tty` on Unix or the console on Windows—rather than the standard streams.
If a controlling terminal is unavailable or `--no-prompt` is passed, the CLI
denies the request for the remainder of the run and prints the corresponding
`--allow` flag to standard error, such as
`sand: denied: read /data/a.txt; pass --allow='read /data/a.txt' to allow it`.
Valid prompt responses are `y` to allow once, `a` to allow always, `n` or an
empty line to deny once, and `N` to deny always; end-of-input denies the request,
and the CLI re-prompts up to three times on unrecognized input before denying.
Before printing a prompt, the CLI escapes control characters, Unicode format
characters, non-ASCII spaces, line/paragraph separators, and combining marks in
the request text, resets terminal attributes on a new line, and prints a
separator line so script output cannot disguise the prompt.

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

Lines are read until the input is complete: while brackets or a string
are open, or the last token cannot end a statement (a binary
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
