+++
title = "Functions"
description = "Declare functions in Sand, call them with named and default arguments, and use lambdas, closures and pipes."
+++

A function declaration names its parameters and their types. The checker infers everything inside the body.
The body is a block, and the value of its last expression is the return value.

```sand
fn hypotenuse(a: float, b: float) -> float {
    (a * a + b * b).sqrt()
}

print(hypotenuse(3.0, 4.0))
```

## Parameters and return types

Every parameter needs a type. The return type follows `->`.
If you omit it, the function returns `()` and Sand discards the value of the last expression.

`return` leaves the function early:

```sand
fn describe(n: int) -> str {
    if n < 0 { return "negative" }
    if n == 0 { "zero" } else { "positive" }
}

print(describe(-5))
print(describe(8))
```

You can call a function before its declaration in the file.

## Named and default arguments

A parameter can have a default value. A default can refer to earlier parameters.
At the call, positional arguments come first, then named arguments in any order:

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

Named arguments make a call such as `http.get(url, timeout: 5s)` clear without a comment.

## Functions are values

A function name is a value of a function type such as `fn(int) -> int`. You can store it or pass it to another function:

```sand
fn double(x: int) -> int { x * 2 }

fn apply_twice(f: fn(int) -> int, x: int) -> int { f(f(x)) }

let op = double
print(op(5))
print(apply_twice(double, 5))
print([1, 2, 3].map(double))
```

A call through a function value takes positional arguments only:

```sand error
fn greet(name: str, greeting: str = "Hello") -> str { "\(greeting), \(name)" }

let g = greet
print(g("Ada", greeting: "Hi"))
```

## Lambdas

A lambda is an anonymous function, written with `=>`:

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

The checker infers the parameter types from the context, usually the function that receives the lambda.
If the context does not give a type, add an annotation:

```sand error
let size = x => x.len()
```

## Closures

A lambda can use the variables around it. It copies the value of each `let` binding when Sand creates the lambda.
It shares each `var` binding, so a change on one side is visible on the other:

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

Each loop iteration has its own `i`, so each lambda keeps a different value.

A closure can keep a `var` alive after its function returns. This makes a counter with private state:

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

## Pipes

`x |> f` calls `f(x)`. `x |> f(a)` calls `f(x, a)`. Pipes let you read a sequence of calls from left to right:

```sand
fn double(x: int) -> int { x * 2 }

fn add(x: int, y: int) -> int { x + y }

print(5 |> double |> add(1))
print(add(double(5), 1))
```

The two lines compute the same value.

## Local functions and recursion

You can declare a function inside a block. A local function can use the variables around it and can call itself:

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

Top-level functions can call each other in any order.
A call in tail position, such as `return f(x)`, does not use stack space, so a tail-recursive loop can run for any number of steps:

```sand
fn sum_to(n: int, acc: int = 0) -> int {
    if n == 0 { acc } else { sum_to(n - 1, acc: acc + n) }
}

print(sum_to(1_000_000))
```

Other recursion has a depth limit (10,000 calls by default). A deeper recursion stops the script with a `stack overflow` error.

## Generic functions

A type parameter in `<...>` lets one function work for many types.
A bound after a colon states what the type must support:

```sand
fn largest<T: Ord>(xs: [T], fallback: T) -> T {
    var best = fallback
    for x in xs {
        if x > best { best = x }
    }
    best
}

print(largest([3, 9, 2], 0))
print(largest(["pear", "fig"], ""))
```

The built-in bounds are `Eq` (`==`), `Ord` (`<` and the other comparisons), `Hash` (map keys and set elements) and `Num` (arithmetic).
Combine bounds with `+`, as in `T: Ord + Hash`. Your own traits are bounds too. The [Traits](/guide/traits/) chapter shows how to write them.
