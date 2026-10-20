+++
title = "Control flow"
description = "Blocks, if, match and loops in Sand, which are expressions that produce values."
+++

In Sand, blocks, `if`, `match` and `loop` are expressions: each one produces a value.
You can assign the result directly, without a temporary `var`.

## Blocks

A block `{ ... }` runs its statements in order. Its value is the value of its last expression:

```sand
let area = {
    let width = 4
    let height = 3
    width * height
}
print(area)
```

The bindings inside the block do not exist outside it.
A block that ends with a statement, such as an assignment, has the value `()`.

## if and else

`if` takes a `bool` condition. Sand has no "truthy" values, so `if count { ... }` with an `int` is a type error.

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

When you use `if` as a value, both branches must have the same type:

```sand error
let label = if true { 1 } else { "one" }
```

An `if` without `else` has the type `()`.

## if let

`if let` runs a branch when a value matches a pattern, and binds the parts of the value.
Use it to take the value out of an `Option`. Sand has no null; a value that can be absent has an option type such as `int?`.

```sand
let ages = ["ada": 36, "alan": 41]

if let Some(age) = ages.get("ada") {
    print("ada is \(age)")
}

let grace = if let Some(age) = ages.get("grace") { age.to_str() } else { "unknown" }
print(grace)
```

`let ... else` binds a pattern or leaves the current scope. Its `else` block must not complete normally; it must `return`, `break`, `continue` or fail:

```sand
fn first_word(text: str) -> str {
    let Some(word) = text.split_whitespace().first() else { return "(empty)" }
    word
}

print(first_word("hello world"))
print(first_word("   "))
```

The [Errors](/guide/errors/) chapter shows more ways to work with `Option` and `Result`.

## A first look at match

`match` compares a value with a list of patterns, from top to bottom, and runs the first arm that matches.
A pattern can be a literal, a list of alternatives with `|`, a binding with a guard after `if`, or `_` for any value:

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

The arms must cover every possible value. The checker rejects a `match` that misses a case:

```sand error
let n = 3
let name = match n {
    0 => "zero"
    1 => "one"
}
```

`match` is most useful with enums and structs. The [Patterns](/guide/patterns/) chapter covers it in full.

## loop

`loop` repeats its body until a `break`. `break value` makes `value` the result of the loop:

```sand
var n = 1
let first = loop {
    if n * n > 50 { break n }
    n += 1
}
print(first)
```

## while and while let

`while` repeats its body while a condition is true.
`while let` repeats it while a value matches a pattern:

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

`while` and `for` loops have the value `()`. Only `loop` can `break` with a value.

## for

`for` iterates over a range, a list, a map, a set or an iterator. `a..b` excludes `b` and `a..=b` includes it:

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

The loop variable can be a pattern, as in `(city, temp)`. A map gives its entries as `(key, value)` tuples, in insertion order.

`continue` goes to the next iteration, and `break` ends the loop:

```sand
for i in 0..10 {
    if i % 3 != 0 { continue }
    if i > 6 { break }
    print(i)
}
```

## Labels

`break` and `continue` apply to the innermost loop. To leave an outer loop, give it a label with `@name`:

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

`break @name value` is valid only when `@name` labels a `loop`.
