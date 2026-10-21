+++
title = "Control flow"
description = "Blocks, if, match, and loops."
+++

In Sand, blocks, `if`, `match`, and `loop` are expressions that produce values. You can bind their results directly to a `let` variable without initializing a mutable `var`.

## Blocks

In a block `{ ... }`, statements execute in order and the final expression returns the value of the block:

```sand
let area = {
    let width = 4
    let height = 3
    width * height
}
print(area)
```

Bindings declared inside a block are scoped to that block. A block that ends with a statement, such as an assignment, returns `()`.

## if and else

An `if` expression requires a `bool` condition. Non-boolean values are not implicitly converted.

```sand
let n = 7
let parity = if n % 2 == 0 { "even" } else { "odd" }
print(parity)

if n > 5 {
    print("large")
} else if n > 0 {
    print("small")
} else {
    print("not positive")
}
```

If an `if` expression is used as a value, both branches must have the same type:

```sand error
let label = if true { 1 } else { "one" }
```

An `if` expression without an `else` branch has type `()`.

## if let

`if let` tests whether a value matches a pattern and binds the matched fields inside the branch. Use `if let` to unwrap optional values (`T?`):

```sand
let ages = ["ada": 36, "alan": 41]

if let Some(age) = ages.get("ada") {
    print("ada is \(age)")
}

let grace = if let Some(age) = ages.get("grace") { age.to_str() } else { "unknown" }
print(grace)
```

`let ... else` binds a refutable pattern in the current scope or diverges (`return`, `break`, `continue`, or `fail`) in the `else` block:

```sand
fn first_word(text: str) -> str {
    let Some(word) = text.split_whitespace().first() else { return "(empty)" }
    word
}

print(first_word("hello world"))
print(first_word("   "))
```

See [Errors](/guide/errors/) for additional ways to work with `Option` and `Result`.

## A first look at match

`match` compares a value against a sequence of patterns from top to bottom and evaluates the first matching arm:

```sand
fn size(n: int) -> str {
    match n {
        0 => "none"
        1 | 2 | 3 => "few"
        k if k < 0 => "invalid"
        _ => "many"
    }
}

print(size(2))
print(size(-1))
print(size(40))
```

Every `match` must be exhaustive; the compiler rejects a `match` that does not cover all possible values:

```sand error
let n = 3
let name = match n {
    0 => "zero"
    1 => "one"
}
```

See [Pattern matching](/guide/patterns/) for full details on patterns and exhaustiveness.

## loop

`loop` repeats its body until exited with `break`. Passing a value to `break` sets the result of the `loop` expression:

```sand
var n = 1
let first = loop {
    if n * n > 50 { break n }
    n += 1
}
print(first)
```

## while and while let

`while` repeats its body while a `bool` condition is true, and `while let` repeats its body while a value matches a pattern:

```sand
var steps = 0
var x = 27
while x != 1 {
    x = if x % 2 == 0 { x / 2 } else { 3 * x + 1 }
    steps += 1
}
print(steps)

let stack = [1, 2, 3]
while let Some(top) = stack.pop() {
    print(top)
}
```

`while` and `for` loops always evaluate to `()`; only `loop` can return a value via `break`.

## for

`for` iterates over ranges (`a..b` exclusive, `a..=b` inclusive), lists, maps, sets, and iterators:

```sand
for i in 1..=3 {
    print(i)
}

for name in ["ada", "alan"] {
    print(name.to_upper())
}

for (city, temp) in ["Oslo": -3, "Lima": 19] {
    print("\(city): \(temp)")
}

for (i, letter) in ["a", "b"].enumerate() {
    print("\(i) \(letter)")
}
```

The loop binding can be any irrefutable pattern, such as `(city, temp)` for map entries in insertion order.

Use `continue` to skip to the next iteration and `break` to exit the loop early:

```sand
for i in 0..10 {
    if i % 3 != 0 { continue }
    if i > 6 { break }
    print(i)
}
```

## Labels

Prefix a loop with `@label` to target an outer loop from `break` or `continue`:

```sand
let grid = [[1, 2, 3], [4, -5, 6], [7, 8, 9]]

@rows for row in grid {
    for x in row {
        if x < 0 {
            print("found \(x)")
            break @rows
        }
    }
}

let target = 8
let position = @search loop {
    for (r, row) in grid.enumerate() {
        if let Some(c) = row.index_of(target) { break @search (r, c) }
    }
    break @search (-1, -1)
}
print(position)
```

`break @label value` is valid only if `@label` is attached to a `loop` expression.
