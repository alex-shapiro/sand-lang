+++
title = "Traits and generics"
description = "Write Sand code that works for many types with generic functions, built-in bounds and traits, and implement traits for your own and built-in types."
+++

A generic function works for many types. A bound on a type parameter says what the function needs from the type, for example that values can be compared. Sand has four built-in bounds. A trait is a bound that you define yourself: a set of methods that a type implements.

## Generic functions

Put type parameters in `<...>` after the function name. Sand infers the type arguments at each call.

```sand
fn first_or<T>(xs: [T], default: T) -> T {
    xs.first() ?? default
}

print(first_or([3, 4], 0))
print(first_or([], "none"))
```

Structs and enums can also be generic. [Structs and enums](/guide/structs-and-enums/#generic-types) shows examples.

## Built-in bounds

Without a bound, a function can store, pass and print a `T` value, but it cannot compare or add it. To compare, hash or add values, add a bound with `T: Bound`. Combine bounds with `+`, as in `T: Ord + Hash`.

| Bound  | Lets you                          | Examples of types              |
| ------ | --------------------------------- | ------------------------------ |
| `Eq`   | use `==` and `!=`                 | all types except functions     |
| `Ord`  | use `<`, `>`, `sorted()`, `max()` | numbers, `str`, lists, tuples  |
| `Hash` | use values as map keys or in sets | `int`, `bool`, `str`, tuples   |
| `Num`  | use arithmetic, `T.zero()`        | `int`, `float`                 |

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

Structs, enums and tuples satisfy a bound when all their parts do. A struct with a `var` field is not `Hash`. [§5.6 of the specification](https://github.com/alex-shapiro/sand-lang/blob/main/docs/SPEC.md#56-bounds) gives the full rules.

If the function uses an operation that its bounds do not allow, the compiler tells you which bound to add:

```sand error
fn bigger<T>(a: T, b: T) -> T {
    if a > b { a } else { b }
}
```

## Defining traits

A trait lists methods. A method without a body is required: each type must define it. A method with a body is a default method: types get it without writing it, and they can replace it.

```sand
/// Things with a short text description.
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

An `impl Trait for Type` block defines the trait's methods for one type. Each method must have the same signature as in the trait. Inside a trait, `Self` stands for the type that implements it.

### Static methods

A trait method without `self` is static. Call it on the type, on a type parameter, or on `Self`. Sand can also infer the type from the expected result.

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
let nothing: str = Monoid.empty()
print(nothing.len())
```

### Supertraits

A trait can require other traits, or built-in bounds, with `trait Name: Other`. A type must implement the supertraits before it can implement the trait. Default methods can then call the supertrait methods.

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

You can implement your own traits for built-in types such as `bool`, `str` and `[T]`. A generic impl can have bounds. Then it applies only to the types that satisfy them. Here, `[T]` implements `Describe` exactly when `T` does:

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

This is called a conditional impl. A call with `[int]` is a compile error, because `int` does not implement `Describe`.

## Implementing `Iterator<T>`

The prelude trait `Iterator<T>` has one required method, `next`. Implement it, and you can use your type in `for` loops. Its default method `iter()` gives a lazy `Iter<T>` with adapters such as `map` and `filter`.

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

The [Iterator reference](/std/prelude/Iterator/) lists the adapters and consumers.

## Qualified calls

You can call a trait method through the trait, as `Trait.method(value)`, or through the type, as `Type.method(value)`. Use this form when two traits give a type methods with the same name. A plain method call is then ambiguous, and the compiler reports an error.

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

## Where you can write an impl

An impl must be in the module that declares the trait, or in the module that declares the type. This is the orphan rule. The prelude declares the built-in types and traits, so your modules cannot implement a prelude trait for a built-in type:

```sand error
impl Close for int {
    fn close(self) {}
}
```

An impl has no name and no visibility. When Sand loads its module, the impl applies to the whole program.

## No trait objects

A trait is a bound, not a type. You cannot make a list of "any type that implements `Shape`". To keep values of different types together, use an enum with one variant for each type:

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

The compiler then checks that each `match` handles every kind of shape.
