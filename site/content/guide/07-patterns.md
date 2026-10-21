+++
title = "Pattern matching"
description = "Use match, if let, while let and let-else in Sand to test the shape of a value and take it apart, with exhaustiveness checked at compile time."
+++

A pattern describes the shape of a value. If the value has that shape, the pattern matches and binds names to its parts. Sand uses patterns in `match`, `let`, `if let`, `while let` and `for`. The compiler makes sure that every `match` covers all possible values.

## `match`

A `match` compares a value against its arms from top to bottom. It runs the body of the first arm whose pattern matches. A `match` is an expression, so it has a value.

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

Separate arms with newlines or commas. The `_` pattern matches any value and binds nothing.

## Pattern forms

### Literals and ranges

Integer, string and `bool` literals match equal values. An inclusive range `a..=b` matches the integers from `a` to `b`. Sand does not support exclusive ranges (`a..b`) in patterns.

### Bindings and tuples

A name matches any value and binds it. A tuple pattern matches each element.

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

The second arm is an or-pattern: it matches if one of its alternatives matches. All alternatives must bind the same names with the same types. The third arm has a guard, described below.

### Enum variants

A variant pattern matches one variant and takes its payload apart by position. You can omit the enum name, because Sand knows the type of the value.

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

`Some(x)`, `None`, `Ok(x)` and `Err(e)` are variant patterns too.

### Structs

A struct pattern names the type and some of its fields. A field can have a sub-pattern (`admin: true`) or only a name, which binds the field. End the pattern with `..` to ignore the other fields. Without `..`, the pattern must mention every field.

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

A list pattern matches a list of a given length. A rest pattern `..` matches the remaining elements, and `..name` binds them as a new list.

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

A guard is an `if` condition after the pattern. The arm matches only if the pattern matches and the condition is true. The guard can use the names that the pattern binds.

The compiler cannot know the result of a guard. Thus a guard does not help to make a `match` exhaustive:

```sand error
fn sign(n: int) -> int {
    match n {
        x if x > 0 => 1
        x if x <= 0 => 0
    }
}
```

## Exhaustiveness

Every `match` must cover all values of its type. If it does not, the compiler shows one value that no arm matches:

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

The check also looks inside tuples, variants and lists. For types with very many values, such as `int` and `str`, add a `_` arm or a binding arm. For lists, `[]` and `[_, .._]` together cover all lists.

An arm that can never match, because earlier arms cover all its values, is also an error:

```sand error
fn f(n: int) -> str {
    match n {
        _ => "any"
        0 => "zero"
    }
}
```

## `let` with patterns

A `let` can take a value apart with a pattern. The pattern must be irrefutable: it must match every value of the type.

```sand
let pair = ("ann", 31)
let (name, age) = pair
print("\(name) is \(age)")
```

### `let ... else`

If the pattern can fail, add an `else` block. The block runs when the pattern does not match, and it must leave the current scope, for example with `return`, `break` or `fail`. After the statement, the bindings are available in the rest of the block.

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

This form keeps the main path of a function at the left margin. [Errors](/guide/errors/) shows `?`, which is shorter when you only pass on a `None` or an `Err`.

### `if let` and `while let`

`if let` and `while let` also take a pattern that can fail. [Control flow](/guide/control-flow/#if-let) shows both.

## Patterns in `for`

The binding of a `for` loop is a pattern. Map iteration gives `(key, value)` tuples, and `enumerate` gives `(index, element)` tuples.

```sand
let ages = ["ann": 31, "bo": 12]
for (name, age) in ages {
    print("\(name): \(age)")
}
for (i, name) in ["x", "y"].enumerate() {
    print("\(i) \(name)")
}
```

As in `let`, the pattern of a `for` loop must be irrefutable. To skip some elements, use a `match` or an `if let` in the body.
