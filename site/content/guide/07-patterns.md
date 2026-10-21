+++
title = "Pattern matching"
description = "Match patterns, if let, while let, and let-else."
+++

Patterns test the structure of a value and bind variables to its components. Sand supports patterns in `match`, `let`, `if let`, `while let`, and `for`, and verifies at compile time that every `match` expression is exhaustive.

## `match`

A `match` expression tests a value against its arms from top to bottom and evaluates the body of the first matching arm:

```sand
fn describe(n: int) -> str {
    match n {
        0 => "zero"
        -1 => "minus one"
        1..=9 => "a digit"
        _ => "something else"
    }
}

for n in [0, -1, 7, 42] {
    print(describe(n))
}
```

The wildcard pattern `_` matches any value without binding a variable.

## Pattern forms

### Literals and ranges

Integer, string, and `bool` literals match equal values, and inclusive integer ranges (`a..=b`) match any integer from `a` through `b`.

### Bindings and tuples

An identifier pattern matches any value and binds it to that name, and a tuple pattern matches each element positionally:

```sand
fn classify(p: (int, int)) -> str {
    match p {
        (0, 0) => "origin"
        (x, 0) | (0, x) => "on an axis at \(x)"
        (x, y) if x == y => "on the diagonal"
        _ => "elsewhere"
    }
}

print(classify((0, 3)))
print(classify((2, 2)))
print(classify((1, 2)))
```

An or-pattern (`pat1 | pat2`) matches if any alternative matches; every alternative must bind the same variable names with identical types.

### Enum variants

A variant pattern matches a specific enum variant and destructures its payload positionally:

```sand
enum Shape {
    Circle(radius: float)
    Rect(w: float, h: float)
    Empty
}

fn name(s: Shape) -> str {
    match s {
        Circle(r) if r == 0.0 => "a point"
        Circle(_) => "a circle"
        Rect(w, h) if w == h => "a square"
        Rect(_, _) | Empty => "something else"
    }
}

print(name(Shape.Circle(0.0)))
print(name(Shape.Rect(w: 2.0, h: 2.0)))
print(name(Shape.Empty))
```

### Structs

A struct pattern matches fields by name (`field: pattern` or shorthand `field`), and `..` ignores any remaining fields:

```sand
struct User {
    name: str
    age: int
    admin: bool
}

fn greet(u: User) -> str {
    match u {
        User { admin: true, name, .. } => "welcome back, \(name)"
        User { age: 0..=17, .. } => "hi"
        User { name, .. } => "hello, \(name)"
    }
}

print(greet(User { name: "ann", age: 30, admin: true }))
print(greet(User { name: "bo", age: 12, admin: false }))
print(greet(User { name: "cy", age: 40, admin: false }))
```

### Lists

A list pattern matches lists by length and element patterns, with an optional `..` or `..rest` slice pattern:

```sand
fn sum(xs: [int]) -> int {
    match xs {
        [] => 0
        [first, ..rest] => first + sum(rest)
    }
}

fn ends(xs: [str]) -> str {
    match xs {
        [] => "nothing"
        [only] => only
        [a, .., z] => "\(a) to \(z)"
    }
}

print(sum([1, 2, 3]))
print(ends(["a"]))
print(ends(["a", "b", "c"]))
```

## Guards

An `if` guard after a pattern requires a boolean condition to evaluate to `true` for the arm to match. Because guards can execute arbitrary expressions, guarded arms do not contribute toward exhaustiveness checking:

```sand error
fn sign(n: int) -> int {
    match n {
        x if x > 0 => 1
        x if x <= 0 => 0
    }
}
```

## Exhaustiveness

Every `match` expression must cover all possible values of the matched type:

```sand error
enum Light {
    Red
    Yellow
    Green
}

fn next(l: Light) -> Light {
    match l {
        Red => Light.Green
        Green => Light.Yellow
    }
}
```

The compiler also rejects unreachable arms that are already covered by earlier patterns:

```sand error
fn f(n: int) -> str {
    match n {
        _ => "any"
        0 => "zero"
    }
}
```

## `let` with patterns

A plain `let` statement accepts any irrefutable pattern (a pattern that matches all values of its type):

```sand
let pair = ("ann", 31)
let (name, age) = pair
print("\(name) is \(age)")
```

### `let ... else`

To bind a refutable pattern in the current scope, attach an `else` block that diverges (`return`, `break`, `continue`, or `fail`) if the pattern does not match:

```sand
fn parse_pair(text: str) -> (int, int)? {
    let [a, b] = text.split(",") else {
        return None
    }
    let Some(x) = a.trim().parse_int() else {
        return None
    }
    let Some(y) = b.trim().parse_int() else {
        return None
    }
    Some((x, y))
}

print(parse_pair("3, 4"))
print(parse_pair("3"))
print(parse_pair("3, x"))
```

### `if let` and `while let`

`if let` and `while let` conditionally execute a block if a refutable pattern matches (see [Control flow](/guide/control-flow/#if-let)).

## Patterns in `for`

A `for` loop header accepts any irrefutable pattern to destructure each element:

```sand
let ages = ["ann": 31, "bo": 12]
for (name, age) in ages {
    print("\(name): \(age)")
}
for (i, name) in ["x", "y"].enumerate() {
    print("\(i) \(name)")
}
```
