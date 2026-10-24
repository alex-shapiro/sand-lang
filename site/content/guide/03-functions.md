+++
title = "Functions"
description = "Function declarations, arguments, and closures."
+++

A function definition begins with `fn`, followed by the function name and a set of parentheses. Parameters are declared inside the parentheses. After the closing `)`, a function may optionally define a return type with `->` followed by the return type name. After the return type is defined, the function body is declared as a list of statements enclosed in braces.

```sand
fn hypotenuse(a: float, b: float) -> float {
    (a * a + b * b).sqrt()
}

print(hypotenuse(3.0, 4.0))
```

## Parameters and return types

Every parameter requires a type annotation. An optional return type follows `->`. If the return type is omitted, the function returns `()`.

Use `return` to exit a function early:

```sand
fn describe(n: int) -> str {
    if n < 0 { return "negative" }
    if n == 0 { "zero" } else { "positive" }
}

print(describe(-5))
print(describe(8))
```

Functions can be called before their declaration in the same file.

## Named and default arguments

Parameters can define default values, which may reference earlier parameters. Callers pass positional arguments first, followed by named arguments in any order:

```sand
fn area(width: float, height: float = width) -> float {
    width * height
}

fn greet(name: str, greeting: str = "Hello", end: str = "!") -> str {
    "\(greeting), \(name)\(end)"
}

print(area(3.0))
print(area(3.0, height: 2.0))
print(greet("Ada"))
print(greet("Ada", end: "?"))
print(greet(greeting: "Hi", name: "Grace"))
```

## Functions are values

Functions are first-class values with types such as `fn(int) -> int`. They can be stored in variables or passed as arguments:

```sand
fn double(x: int) -> int { x * 2 }

fn apply_twice(f: fn(int) -> int, x: int) -> int { f(f(x)) }

let op = double
print(op(5))
print(apply_twice(double, 5))
print([1, 2, 3].map(double))
```

Calls through a `fn(...)` value accept positional arguments only:

```sand error
fn greet(name: str, greeting: str = "Hello") -> str { "\(greeting), \(name)" }

let g = greet
print(g("Ada", greeting: "Hi"))
```

## Lambdas

A lambda expression defines an anonymous function using `=>`:

```sand
let inc = x => x + 1
let add = (a, b) => a + b
let len = (s: str) => s.len()
let hello = () => "hello"
let clamp = (x: int) => {
    let low = 0
    if x < low { low } else { x }
}

print(inc(1))
print(add(2, 3))
print(len("sand"))
print(hello())
print(clamp(-4))
```

The type checker infers lambda parameter types from surrounding types. If the parameter type cannot be inferred from context, annotate it explicitly:

```sand error
let size = x => x.len()
```

## Closures

A lambda can capture bindings from enclosing scopes. Captured `let` bindings are copied by value at closure creation. Captured `var` bindings are shared with the enclosing scope:

```sand
var count = 0
let tick = () => { count += 1 }
tick()
tick()
print(count)

let actions = []
for i in 0..3 {
    actions.push(() => i * 10)
}
print(actions.map(f => f()))
```

Each iteration of a `for` loop creates a fresh binding. A closure created in a loop captures that iteration's value.

Captured `var` bindings remain alive for the lifetime of the closure:

```sand
fn make_counter() -> fn() -> int {
    var n = 0
    () => {
        n += 1
        n
    }
}

let next_id = make_counter()
print(next_id())
print(next_id())
```

## Local functions and recursion

Functions can be declared inside any block, capture enclosing bindings, and call themselves recursively:

```sand
fn count_paths(width: int, height: int) -> int {
    fn paths(x: int, y: int) -> int {
        if x == width or y == height { return 1 }
        paths(x + 1, y) + paths(x, y + 1)
    }
    paths(0, 0)
}

print(count_paths(3, 3))
```

Calls in tail position reuse the current call frame if the function does not have an active `defer` or `use` scope:

```sand
fn sum_to(n: int, acc: int = 0) -> int {
    if n == 0 { acc } else { sum_to(n - 1, acc: acc + n) }
}

print(sum_to(1_000_000))
```

Non-tail recursion is bounded by the engine's call depth limit. The default limit is 10,000.

## Generic functions

Functions can declare type parameters and the traits that they require in `<...>`. See [Traits and generics](/guide/traits/) for more.
