+++
title = "Traits and generics"
description = "Generic functions, bounds, and traits."
+++

A generic function operates on values of any type that implements the traits of its bounds. A trait specifies the operations that a type must support. The prelude declares built-in traits, and you can define your own.

## Generic functions

Place type parameters in `<...>` after the function name. Sand infers the type arguments at each call site.

```sand
fn first_or<T>(xs: [T], default: T) -> T {
    xs.first() ?? default
}

print(first_or([3, 4], 0))
print(first_or([], "none"))
```

Structs and enums can also be generic (see [Structs and enums](/guide/structs-and-enums/#generic-types)).

## Built-in traits

An unconstrained type parameter `T` supports only storage, passing, and formatting. Add a bound (`T: Trait`) to compare, hash, or perform arithmetic on values of type `T`. Combine multiple bounds with `+`, as in `T: Ord + Hash`.

| Trait    | Operations                       | Supporting types                                                |
| -------- | -------------------------------- | --------------------------------------------------------------- |
| `Eq`     | `==`, `!=`                       | all types except functions                                      |
| `Ord`    | `<`, `>`, `sorted()`, `max()`    | numbers, `str`, lists, tuples                                   |
| `Hash`   | map keys and set elements        | `int`, `bool`, `str`, tuples                                    |
| `Num`    | `+`, `-`, `*`, `/`, `T.zero()`   | `int`, `float`                                                  |
| `Encode` | `json.encode()`, `json.decode()` | all types except functions, tasks, channels, and native objects |

```sand
fn largest<T: Ord>(xs: [T]) -> T? {
    var best = xs.first()?
    for x in xs {
        if x > best {
            best = x
        }
    }
    Some(best)
}

fn total<T: Num>(xs: [T]) -> T {
    xs.fold(T.zero(), (a, b) => a + b)
}

fn counts<T: Hash>(xs: [T]) -> [T: int] {
    let out: [T: int] = [:]
    for x in xs {
        out[x] = (out.get(x) ?? 0) + 1
    }
    out
}

print(largest(["pear", "apple"]))
print(total([1.5, 2.5]))
print(counts(["a", "b", "a"]))
```

The compiler derives `Eq`, `Ord`, `Hash`, and `Encode` for each struct, enum, and tuple whose fields implement them. A struct with a `var` field does not implement `Hash`. `help(Type)` lists the traits that a type implements. For complete rules, see [§5.6 of the specification](/spec/#56-built-in-traits).

If a function uses an operation that its bounds do not permit, the compiler reports the missing bound:

```sand error
fn bigger<T>(a: T, b: T) -> T {
    if a > b { a } else { b }
}
```

## Defining traits

A `trait` block declares a set of methods. A method without a body is required: each implementing type must define it. A method with a body is a default method: implementing types inherit it automatically and can override it.

```sand
/// Values with a short text description.
trait Describe {
    fn describe(self) -> str

    fn shout(self) -> str {
        self.describe().to_upper() + "!"
    }
}

struct Point {
    x: int
    y: int
}

impl Describe for Point {
    fn describe(self) -> str {
        "point at (\(self.x), \(self.y))"
    }
}

fn show<T: Describe>(x: T) {
    print(x.describe())
}

let p = Point { x: 1, y: 2 }
show(p)
print(p.shout())
```

An `impl Trait for Type` block defines the trait methods for a specific type. Each method signature must match the declaration in the trait. Inside a trait, `Self` refers to the implementing type.

### Static methods

A trait method without a `self` parameter is a static method. Call it on a concrete type, on a type parameter, or on `Self`. Sand can also infer the implementing type from the expected return type.

```sand
trait Monoid {
    fn empty() -> Self
    fn combine(self, other: Self) -> Self
}

impl Monoid for int {
    fn empty() -> int {
        0
    }

    fn combine(self, other: int) -> int {
        self + other
    }
}

impl Monoid for str {
    fn empty() -> str {
        ""
    }

    fn combine(self, other: str) -> str {
        self + other
    }
}

fn concat_all<T: Monoid>(xs: [T]) -> T {
    xs.fold(T.empty(), (acc, x) => acc.combine(x))
}

print(concat_all([1, 2, 3]))
print(concat_all(["a", "b", "c"]))
let empty_str: str = Monoid.empty()
print(empty_str.len())
```

### Supertraits

A trait can require other traits, including built-in traits, with `trait Name: Super`. A type must implement all supertraits before it can implement the subtrait. Default methods in the subtrait can call methods from the supertraits.

```sand
trait Named {
    fn name(self) -> str
}

trait Greeter: Named {
    fn greet(self) -> str {
        "hello, \(self.name())"
    }
}

struct Dog {
    nick: str
}

impl Named for Dog {
    fn name(self) -> str {
        self.nick
    }
}

impl Greeter for Dog {}

print(Dog { nick: "rex" }.greet())
```

## Impls for built-in types

You can implement custom traits for built-in types such as `bool`, `str`, and `[T]`. A generic `impl` can declare bounds on its type parameters so that the implementation applies only if the element types implement those traits:

```sand
trait Describe {
    fn describe(self) -> str
}

impl Describe for bool {
    fn describe(self) -> str {
        if self { "yes" } else { "no" }
    }
}

impl<T: Describe> Describe for [T] {
    fn describe(self) -> str {
        self.map(x => x.describe()).join(", ")
    }
}

print([true, false].describe())
print([[true], [false, true]].describe())
```

Calling `.describe()` on `[int]` is a compile error because `int` does not implement `Describe`.

## Implementing `Iterator<T>`

The prelude trait `Iterator<T>` requires a `next` method. Any type that implements `Iterator<T>` can be used in a `for` loop. The default method `iter()` returns a lazy `Iter<T>` that provides adapters such as `map` and `filter`.

```sand
struct Countdown {
    var n: int
}

impl Iterator<int> for Countdown {
    fn next(self) -> int? {
        if self.n <= 0 {
            return None
        }
        self.n -= 1
        Some(self.n + 1)
    }
}

for x in (Countdown { n: 3 }) {
    print(x)
}
print(Countdown { n: 4 }.iter().map(x => x * 10).to_list())
```

Enclose a struct literal in parentheses if it appears directly in a `for` loop header. For the complete list of iterator adapters and consumers, see the [`Iterator` reference](/std/prelude/Iterator/).

## Qualified calls

You can call a trait method through the trait (`Trait.method(value)`) or through the type (`Type.method(value)`). If two traits define a method with the same name on the same type, calling `value.method()` is ambiguous. Qualify the call with the trait name to select the method:

```sand
trait English {
    fn hello(self) -> str
}

trait French {
    fn hello(self) -> str
}

struct Guest {
    name: str
}

impl English for Guest {
    fn hello(self) -> str {
        "hello, \(self.name)"
    }
}

impl French for Guest {
    fn hello(self) -> str {
        "bonjour, \(self.name)"
    }
}

let g = Guest { name: "ann" }
print(English.hello(g))
print(French.hello(g))
```

## Orphan rule

An `impl` block must appear in the module that declares the trait or in the module that declares the target type. Because the prelude declares both the built-in types and the prelude traits, a user module cannot implement a prelude trait for a built-in type:

```sand error
impl Close for int {
    fn close(self) {}
}
```

Trait implementations are always global once their defining module is loaded.

## Heterogeneous collections

A trait is a type bound, not a value type. To store values of different concrete types in a single collection, wrap the types in an enum:

```sand
import math

struct Circle {
    r: float
}

struct Square {
    side: float
}

enum Shape {
    Round(Circle)
    Boxy(Square)

    fn area(self) -> float {
        match self {
            Round(c) => math.pi * c.r * c.r
            Boxy(s) => s.side * s.side
        }
    }
}

let shapes = [Shape.Round(Circle { r: 1.0 }), Shape.Boxy(Square { side: 2.0 })]
print(shapes.map(s => s.area()))
```
