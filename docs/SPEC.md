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
parentheses.

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
and as break continue else enum extend false fn for from if import in let
loop match native not or pub return self Self spawn struct true type
var while
```

`test` is a contextual keyword: it begins a test block only when followed by a
string literal at the top level of a module.

By convention (enforced by the linter): types, enum variants and type
parameters are `UpperCamelCase`; functions, variables, fields, and modules are
`snake_case`; top-level `let` constants may also be `SCREAMING_SNAKE_CASE`. The identifier `_` is a wildcard and may not be read.

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
otherwise). Every script import is checked against the policy as
`Import(path)` (§14).

A module name (`json`) refers first to a host-registered native module, then to a
standard-library module. Only items marked `pub` are visible to importers.
Accessing a non-`pub` item is an error. Importing a name that is not exported is
an error.

A module-qualified name is written `module.name`; a module-qualified type is
written `module.Type`.

### 3.3 Visibility

`pub` may precede `fn`, `struct`, `enum`, `type`, `let` and `var` at the top
level. Fields and methods of a public type are public. Top-level `var`s are
exported read-only: importers may read but not assign them.

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
  Bounds are `Eq`, `Ord`, `Hash` and `Num` (§5.6). Bounds may also be given
  in a trailing `where` clause (`fn sum(self) -> T where T: Num`), which may
  also constrain the type parameters of an enclosing generic `extend` block for
  that one method.
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
types (`extend str { ... }`, `extend<T> [T] { ... }`).

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

Map keys and set elements must be `Hash`.

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
  when the loop starts. Channels are consumed live.
- `break` and `continue` refer to the innermost loop. `return e` returns from
  the innermost function or lambda. All three have type `never`.

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
```

- Rebinding a name with `let` in the same block shadows it.
- Only `var` variables, `var` fields, list elements and map entries can be
  assigned. Assigning a `let` binding or non-`var` field is a compile error.
- `let ... else` requires the `else` block to have type `never`.
- An expression statement whose value has type `Result<_, _>` and is unused
  is reported by the linter (`unused-result`).

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
(`unreachable pattern`).

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
  `m`, `e.m(...)` calls the field's value. Every type also has the universal
  methods `to_str() -> str` (display form) and `debug() -> str` (debug form).
- Integer and float literals have types `int` and `float` respectively; there
  is no literal polymorphism.
- At the end of checking a function, every type variable must be resolved,
  except those bound to the function's own type parameters. Unresolved
  variables are errors (`cannot infer type`).
- Bounds on type parameters are checked at instantiation.
- An expression of type `never` unifies with any type.

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
Panics cannot be caught by scripts. (The test runner isolates panics per test.)

### 10.4 Exit

`exit(code: int) -> never` stops the engine run immediately with the given exit
code. It does not terminate the host process.

### 10.5 Functions and closures

Calls are by value (references for reference types). Recursion depth is bounded
by the `max_call_depth` limit (default 10,000); exceeding it panics with
`stack overflow`. Tail calls (`return f(x)` or a call in tail position of the
function body) do not consume stack depth.

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
  boundary; pending I/O is dropped.
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
| `sum() -> T`, `product() -> T`                                                  | `T: Num`; `[].sum() == 0`     |
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

### 12.9 `[K: V]`

`len`, `is_empty`, `get(k) -> V?`, `get_or(k, default: V) -> V`,
`contains(k) -> bool`, `insert(k, v) -> V?` (returns old value), `remove(k) -> V?`,
`keys() -> [K]`, `values() -> [V]`, `entries() -> [(K, V)]`, `clear()`,
`copy()`, `map_values<W>(f: fn(V) -> W) -> [K: W]`, `filter(f: fn(K, V) -> bool)
-> [K: V]`, `extend(other: [K: V])`, `get_or_insert(k, default: V) -> V` (inserts
when absent and returns the stored value).

### 12.10 `Set<T>`

`len`, `is_empty`, `add(x) -> bool` (true if newly added), `remove(x) -> bool`,
`contains(x)`, `to_list() -> [T]`, `union(o)`, `intersection(o)`,
`difference(o)`, `is_subset(o)`, `copy()`, `clear()`.

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
`reversed() -> [int]`.

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
| `remove_file(path)`, `remove_dir(path, recursive: bool = false)`                 | `FsWrite`                         |
| `rename(from, to) -> Result<()>`                                                 | `FsWrite` on both                 |
| `copy(from, to) -> Result<()>`                                                   | `FsRead` on from, `FsWrite` on to |
| `metadata(path) -> Result<Metadata>`                                             | `FsRead`                          |
| `join(parts: [str]) -> str`, `basename(p)`, `dirname(p)`, `extension(p) -> str?` | none                              |

`struct Metadata { size: int, is_file: bool, is_dir: bool, modified: time.DateTime? }`.

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
errors. Capability: `Net(host:port)`.

Server:

```
pub fn serve(addr: str) -> Result<Server>        // capability NetListen(addr)
server.accept() -> Result<Request>
server.addr() -> str
struct Request { method: str, path: str, query: str, headers: [str: str], body: bytes }
request.respond(status: int, body: bytes, headers: [str: str] = [:]) -> Result<()>
request.text() -> Result<str>
```

### 13.6 `io`

`read_line() -> Result<str?>` (stdin, without the line terminator; `None` at
EOF; capability `Stdin`), `read_all() -> Result<str>` (`Stdin`), `write(s: str)`
(stdout, no newline), `ewrite(s: str)` (stderr), `flush()`.

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

Reading and writing on the same connection from different tasks is allowed.
Upgrading a connection to TLS: `net.connect_tls(host: str, port: int) ->
Result<Conn>` (feature `tls`).

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
Compiled program size is limited to protect the host.

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
cases: int = 100)` (runs `prop` on generated values; on failure panics with the
failing input's debug form). Failure messages for `assert_eq` have the form
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
  escape an allowed directory via `..` or symlinks.

### 14.2 Capabilities

| Capability                  | Raised by                                  |
| --------------------------- | ------------------------------------------ |
| `FsRead(path)`              | reading files, listing dirs, metadata      |
| `FsWrite(path)`             | creating, writing, removing, renaming      |
| `Net(host, port)`           | outgoing TCP/HTTP connections              |
| `NetListen(addr)`           | listening sockets                          |
| `Env(name)`                 | environment variable reads (`"*"` for all) |
| `Stdin`, `Stdout`, `Stderr` | standard streams                           |
| `Import(path)`              | loading a script module                    |
| `Custom(name, detail)`      | host native modules may define their own   |

### 14.3 Decisions

A policy maps a request to `Allow`, `Deny` or `Prompt`. On `Prompt` the engine
calls the host's prompter, suspending **all** script execution until it
answers `AllowOnce`, `AllowAlways`, `DenyOnce` or `DenyAlways`. "Always" answers
are remembered for the identical request for the lifetime of the engine. If no
prompter is configured, `Prompt` behaves as `Deny`.

### 14.4 Effects of a denial

- I/O functions return `Err` with kind `"permission_denied"`.
- `env.get` returns `None`.
- `print`, `eprint` and `io.write` panic with `permission denied: stdout` /
  `stderr`.
- A denied `Import` is a compile-time error.

### 14.5 Built-in policies

- `Policy::deny_all()` — everything denied except `Stdout` and `Stderr`.
- `Policy::allow_all()` — everything allowed (for trusted scripts).
- `RulePolicy` — a rule list: allowed/denied read paths, write paths, network
  hosts (`host`, `host:port`, `*.example.com`), listen addresses, environment
  variable names, streams, imports, and a fallback decision for unmatched
  requests (`Deny` or `Prompt`). Deny rules take precedence over allow rules.

---

## 15. Resource limits

Configurable per engine:

| Limit                | Default   | Effect when exceeded                                                                                            |
| -------------------- | --------- | --------------------------------------------------------------------------------------------------------------- |
| `max_memory`         | 512 MiB   | run fails: `memory limit exceeded`                                                                              |
| `max_instructions`   | unlimited | run fails: `instruction limit exceeded` (counted in fuel units: one per call, loop iteration and backward jump) |
| `max_cpu_time`       | unlimited | run fails: `cpu time limit exceeded` (time spent executing, excluding waits and prompts)                        |
| `max_wall_time`      | unlimited | run fails: `time limit exceeded`                                                                                |
| `max_call_depth`     | 10,000    | panic: `stack overflow`                                                                                         |
| `max_tasks`          | 10,000    | `spawn` panics: `task limit exceeded`                                                                           |
| `max_open_resources` | 1,024     | open/connect/listen fail with kind `"limit"`                                                                    |
| `max_output_bytes`   | unlimited | writes fail: `output limit exceeded`                                                                            |
| `max_http_body`      | 64 MiB    | HTTP read fails with kind `"limit"`                                                                             |
| `max_source_bytes`   | 16 MiB    | compile error                                                                                                   |

Memory is measured as the engine's accounting of live heap objects (including
their buffers) and is enforced after garbage collection. Limit violations are
reported to the host as distinct error variants and cannot be intercepted by
scripts. The host may also interrupt a running engine from another thread with
an `InterruptHandle`.

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

---

## 17. Tooling

### 17.1 `sand check FILE...`

Parses and type checks; prints diagnostics; exit code 1 on errors.

### 17.2 `sand lint FILE...`

Reports warnings (exit code 1 if any, unless `--allow-warnings`):

| Lint                | Description                                                                      |
| ------------------- | -------------------------------------------------------------------------------- |
| `unused-variable`   | local binding or parameter never read (names starting with `_` are exempt)       |
| `unused-import`     | import never referenced                                                          |
| `unused-function`   | private top-level function never referenced                                      |
| `unnecessary-var`   | `var` never reassigned (use `let`)                                               |
| `unused-result`     | a `Result` value is discarded                                                    |
| `unreachable-code`  | statement after `return`, `break`, `continue`, `panic`, `exit`                   |
| `naming`            | violates naming conventions (§2.4)                                               |
| `bool-comparison`   | comparing with `true`/`false` literal                                            |
| `self-assignment`   | `x = x`                                                                          |
| `len-zero`          | `x.len() == 0` (use `is_empty()`)                                                |
| `empty-block`       | empty `if`/`else`/loop body                                                      |
| `todo`              | call to `todo()`                                                                 |
| `shadowed-variable` | `let` re-binding a name already bound in the same function with a different type |

A lint can be suppressed for the next line with `// lint: allow(name)`, or for
a file with `//! lint: allow(name)`.

### 17.3 `sand fmt FILE... [--check]`

Rewrites files in canonical style: 4-space indentation, one statement per line,
spaces around binary operators, no space before `(` in calls, `{` on the same
line, trailing commas in multi-line lists/arguments/fields, at most one blank
line between statements, at most two consecutive blank lines never, items
separated by exactly one blank line, line width 100 (calls, lists, maps and
parameter lists that do not fit are broken one element per line), comments
preserved. Formatting is idempotent: `fmt(fmt(x)) == fmt(x)`, and formatting
never changes the parse tree (modulo spans).

### 17.4 `sand test FILE...`

Runs the module's top-level code with output captured, then every `test` block
as a separate task with a fresh view of top-level state _as initialised_. Each
test passes if it completes without panicking. Output: one line per test
(`ok name` / `FAIL name: message`) and a summary `N passed, M failed`.

### 17.5 `sand run FILE [-- ARGS...]`

Options: `--allow-read=PATHS`, `--allow-write=PATHS`, `--allow-net=HOSTS`,
`--allow-listen=ADDRS`, `--allow-env=NAMES`, `--allow-stdin`, `--allow-all`,
`--prompt` (prompt on the terminal for unmatched requests), `--max-memory=BYTES`
(suffixes K, M, G), `--max-time=DURATION`, `--max-cpu=DURATION`,
`--max-instructions=N`, `--seed=N`. Exit code: the script's exit code, 1 for a
runtime error or `?` failure, 2 for compile errors, 3 for limit violations.

### 17.6 `sand repl`

Interactive read-eval-print loop. Each input is checked and run in the
context of the previous inputs.

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

The checker continues after errors where possible and reports up to 50 errors
per run. Parse errors stop checking of the affected module.
