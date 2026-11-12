+++
title = "Basics"
description = "Bindings, primitive types, arithmetic, strings, durations, and printing."
+++

A Sand script consists of statements executed from top to bottom. This chapter introduces comments, bindings, primitive types, operators, string interpolation, and standard output.

## Comments

Line comments start with `//`, and doc comments (`///` or `//!`) attach documentation that `help()` can display at runtime:

```sand
// A line comment.

/// Returns the ultimate answer.
fn answer() -> int { 42 }

help(answer)
```

## Statements and newlines

A newline terminates a statement without requiring a semicolon. A line that begins with `.` or a binary operator continues the expression from the previous line:

```sand
let scores = [72, 95, 88, 41, 63]
let report = scores
    .filter(s => s >= 60)
    .sorted()
    .reversed()
    .map(s => "passing score: \(s)")
    .join(", ")
print(report)
```

## Bindings

`let` declares an immutable binding, and `var` declares a mutable binding:

```sand
let name = "Ada"
var visits = 0
visits += 1
visits += 1
print("\(name) has \(visits) visits")
```

Reassigning a `let` binding is a compile error:

```sand error
let limit = 10
limit = 20
```

Prefer `let` unless the binding must be reassigned. The linter warns if a `var` binding is never mutated.

The type checker infers binding types from their initializers, or you can annotate the type explicitly after a colon:

```sand
let ratio: float = 0.75
print(ratio)
```

Declaring a new `let` binding with the same name in the same scope shadows the previous binding.

## Primitive types

| Type       | Example values                  |
| ---------- | ------------------------------- |
| `int`      | `42`, `-7`, `1_000_000`, `0xff` |
| `float`    | `3.14`, `1e9`, `inf`, `nan`     |
| `bool`     | `true`, `false`                 |
| `str`      | `"hello"`                       |
| `bytes`    | `b"\x00\xff"`                   |
| `duration` | `250ms`, `1.5s`, `2h`           |
| `()`       | `()` (unit value)               |

`int` is a 64-bit signed integer, and `float` is a 64-bit IEEE-754 floating-point number. Float literals require digits on both sides of the decimal point (`1.0` is correct, `1.` and `.5` are not).

## Arithmetic

Supported arithmetic operators are `+`, `-`, `*`, `/`, `%`, and `**` (exponentiation). Both operands must have the same numeric type. Sand does not perform implicit numeric conversions:

```sand error
let count = 3
let half = count / 2.0
```

Convert between numeric types explicitly with `to_float()` and `to_int()`, which truncates toward zero:

```sand
let count = 3
print(count.to_float() / 2.0)
print(3.9.to_int())
```

Integer arithmetic is checked at runtime. Overflow terminates execution with a runtime error:

```sand fails
let big = int.max_value()
print(big + 1)
```

Integer `/` performs floor division, and `%` computes the floor remainder with the sign of the divisor:

```sand
print(7 / 2)
print(-7 / 2)
print(-7 % 2)
print(7 % -2)
```

Integer division by zero terminates execution with a runtime error. Float division by zero produces `inf`, `-inf`, or `nan` per IEEE-754.

Logical operators `and`, `or`, and `not` require `bool` operands. `and` and `or` short-circuit.

## Strings

A `str` value is an immutable UTF-8 string. `len()` returns the byte length, and `char_count()` returns the number of Unicode scalar values:

```sand
let word = "naïve"
print(word.len())
print(word.char_count())
print(word.to_upper())
print("a,b,c".split(","))
```

The `+` operator concatenates two strings. See the [`str` reference](/std/prelude/str/) for all string methods.

### Interpolation

`\(expr)` formats any expression and inserts the result into a string literal:

```sand
let item = "tea"
let cups = 3
print("\(cups) cups of \(item) cost \(cups * 2) coins")
print("list: \([1, 2, 3])")
```

Append `:spec` inside `\(...)` to control formatting, such as `.2` for precision, `>6` for width and alignment, `03` for zero-padding, `x` for hexadecimal, or `?` for debug output:

```sand
let pi = 3.14159
print("pi is about \(pi:.2)")
print("[\(42:>6)]")
print("\(7:03)")
print("\(255:x)")
print("\("tea":?)")
```

See [specification §10.10](/spec/#1010-interpolation-format-specifications) for the full format grammar.

### Multi-line and raw strings

Triple-quoted strings (`"""..."""`) can span multiple lines. They automatically strip the leading newline and common indentation:

```sand
let name = "Sand"
let note = """
    Dear \(name),
      thank you.
    """
print(note)
```

Raw strings (`r"..."` or `r#"..."#`) disable escape sequences and interpolation:

```sand
print(r"C:\temp\(not interpolated)")
print(r#"She said "hi"."#)
```

## Bytes

A `bytes` value is an immutable byte sequence written as `b"..."` with optional `\xNN` hex escapes:

```sand
let data = b"OK\x00\xff"
print(data.len())
print(data.to_hex())
print("hi".bytes())
```

## Durations

A `duration` value represents a signed nanosecond span. A duration literal is written as a number with unit suffix (`ns`, `us`, `ms`, `s`, `m`, `h`, `d`, or `w`):

```sand
let timeout = 1.5s
let total = timeout * 4 + 500ms
print(total)
print(total.millis())
print(2h / 30m)
```

Durations can be added, subtracted, scaled by `int` or `float`, or divided by another `duration` to produce a `float` ratio.

## Unit

`()` is both the unit type and its sole value. Functions without an explicit return type and `if` expressions without an `else` branch evaluate to `()`.

## Printing

`print` writes the display representation of a value followed by a newline to standard output, and `eprint` writes to standard error:

```sand
print("text")
print(3.0)
print(["tea", "milk"])
eprint("a warning")
```
