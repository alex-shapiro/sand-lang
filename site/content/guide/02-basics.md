+++
title = "Basics"
description = "Bindings, primitive types, arithmetic, strings, durations and printing in Sand."
+++

A Sand script is a sequence of statements. Sand runs them from top to bottom.
This chapter shows the parts of the language that every script uses: bindings, the primitive types and their operators.

## Comments

```sand
// A line comment.
/* A block comment. /* Block comments can nest. */ */

/// A doc comment documents the item that follows it.
fn answer() -> int { 42 }

help(answer)
```

`help(answer)` shows the declaration and the doc comment of `answer`.

## Statements and newlines

A newline ends a statement. You do not need semicolons.
A line that starts with `.`, `|>` or a binary operator continues the previous line:

```sand
let scores = [72, 95, 88, 41, 63]
let report = scores
    .filter(s => s >= 60)
    .sorted()
    .reversed()
    .map(s => "score \(s)")
    .join(", ")
print(report)
```

`sand fmt` puts each call of a long chain on its own line, as in this example.

## Bindings

`let` makes an immutable binding. `var` makes a mutable one.

```sand
let name = "Ada"
var visits = 0
visits += 1
visits += 1
print("\(name) has \(visits) visits")
```

The checker rejects an assignment to a `let` binding:

```sand error
let limit = 10
limit = 20
```

Use `let` unless the value must change. The linter reports a `var` that the script never assigns.

Sand infers the type of a binding from its value. To state the type, write it after a colon:

```sand
let ratio: float = 0.75
print(ratio)
```

A new `let` with the same name in the same block shadows the earlier binding.

## Primitive types

| Type       | Example values                  |
| ---------- | ------------------------------- |
| `int`      | `42`, `-7`, `1_000_000`, `0xff` |
| `float`    | `3.14`, `1e9`, `inf`, `nan`     |
| `bool`     | `true`, `false`                 |
| `str`      | `"hello"`                       |
| `bytes`    | `b"\x00\xff"`                   |
| `duration` | `250ms`, `1.5s`, `2h`           |
| `()`       | `()`, the unit value            |

An `int` is a 64-bit signed integer. A `float` is a 64-bit IEEE-754 number.
A float literal must have digits on both sides of the point: write `1.0`, not `1.` or `.5`.

## Arithmetic

The operators are `+`, `-`, `*`, `/`, `%` and `**` (power).
Both operands must have the same type. Sand never converts an `int` to a `float` for you:

```sand error
let count = 3
let half = count / 2.0
```

Convert explicitly with `to_float()` or `to_int()`. `to_int()` truncates toward zero.

```sand
let count = 3
print(count.to_float() / 2.0)
print(3.9.to_int())
```

Integer arithmetic is checked. An overflow stops the script with an error, so a wrong result never goes unnoticed:

```sand fails
let big = int.max_value()
print(big + 1)
```

Integer `/` is floor division and `%` is floor modulo. The result of `%` has the sign of the divisor:

```sand
print(7 / 2)
print(-7 / 2)
print(-7 % 2)
print(7 % -2)
```

Integer division by zero is an error. Float division by zero gives `inf`, `-inf` or `nan`, as IEEE-754 specifies.

`and`, `or` and `not` operate on `bool` values. `and` and `or` evaluate the right operand only if they need it.
Comparisons do not chain: write `0 <= x and x < 10`, not `0 <= x < 10`.

## Strings

A `str` is immutable UTF-8 text. `len()` gives the length in bytes and `char_count()` gives the number of characters:

```sand
let word = "naïve"
print(word.len())
print(word.char_count())
print(word.to_upper())
print("a,b,c".split(","))
```

`+` joins two strings. The [`str` reference](/std/prelude/str/) lists all the methods.

### Interpolation

`\(expr)` inserts the value of an expression into a string. The expression can have any type:

```sand
let item = "tea"
let cups = 3
print("\(cups) cups of \(item) cost \(cups * 2) coins")
print("list: \([1, 2, 3])")
```

A format specification after a colon controls the output.
`.2` sets two digits after the point, `>6` aligns right in six characters, `03` pads with zeros, and `x` gives hexadecimal:

```sand
let pi = 3.14159
print("pi is about \(pi:.2)")
print("[\(42:>6)]")
print("\(7:03)")
print("\(255:x)")
print("\("tea":?)")
```

The `?` type gives the debug form: a string in quotes. [§10.10 of the specification](https://github.com/alex-shapiro/sand-lang/blob/main/docs/SPEC.md#1010-format-specifications) lists all the format options.

### Multi-line and raw strings

A string in `"""` can span many lines. Sand removes the newline after the opening `"""` and the indentation that all lines share:

```sand
let name = "Sand"
let note = """
    Dear \(name),
      thank you.
    """
print(note)
```

The text keeps the newline before the closing `"""`, so `print` writes an empty line after it.

A raw string starts with `r"`. It has no escapes and no interpolation. Add `#` marks to put a `"` in it:

```sand
print(r"C:\temp\(not interpolated)")
print(r#"She said "hi"."#)
```

## Bytes

A `bytes` value is an immutable sequence of bytes. Write it as `b"..."`, with `\xNN` for any byte:

```sand
let data = b"OK\x00\xff"
print(data.len())
print(data.to_hex())
print("hi".bytes())
```

## Durations

A duration is a span of time, stored as a whole number of nanoseconds.
Write it as a number and a unit: `ns`, `us`, `ms`, `s`, `m`, `h`, `d` or `w`.

```sand
let timeout = 1.5s
let total = timeout * 4 + 500ms
print(total)
print(total.millis())
print(2h / 30m)
```

You can add and subtract durations, and multiply or divide them by numbers.
A duration divided by a duration gives a `float`. An `int` plus a duration is a type error.

## Unit

`()` is the unit type. It has one value, also written `()`.
A function without a return type returns `()`, and so does an `if` without an `else`.

## Printing

`print` writes the display form of any value and a newline to standard output. `eprint` writes to standard error.

```sand
print("text")
print(3.0)
print(["tea", "milk"])
eprint("a warning")
```

Strings print without quotes at the top level, and with quotes inside collections.
