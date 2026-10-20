+++
title = "Structs and enums"
description = "Define your own types in Sand with structs, which group named fields, and enums, which hold one of several variants."
+++

Sand has two ways to define a type. A struct groups named fields into one value. An enum holds exactly one of a fixed set of variants, and each variant can carry data.

## Structs

Declare the fields with their types. Create a value with a struct literal, and read a field with `.`.

```sand
struct Point {
    x: float
    y: float
}

let p = Point { x: 3.0, y: 4.0 }
print(p.x)
print(p)
```

A struct literal must give every field that has no default. Separate fields with newlines or commas.

### Mutable fields

Fields are immutable by default. Declare a field with `var` to let code assign it.

```sand
struct Counter {
    name: str
    var count: int = 0
}

let clicks = Counter { name: "clicks" }
clicks.count += 1
clicks.count += 1
print(clicks)
```

The `= 0` is a field default. Sand uses it when a literal does not give the field. The binding `clicks` is a `let`, but its `var` field is still assignable: `var` on a field controls the field, not the binding.

Assignment to a field without `var` is a compile error:

```sand error
struct Point {
    x: float
    y: float
}

let p = Point { x: 3.0, y: 4.0 }
p.x = 0.0
```

### Shorthand and copies

If a variable has the same name as a field, write the name once. Use `..base` to take the remaining fields from another value.

```sand
struct Point {
    x: float
    y: float
}

let x = 1.0
let p = Point { x, y: 2.0 }
let q = Point { y: 9.0, ..p }
print(q)
```

## Methods

Declare methods inside the struct body. A method whose first parameter is `self` is an instance method: call it on a value. A method without `self` is a static method: call it on the type. Inside the body, `Self` is the struct type.

```sand
struct Vec2 {
    x: float
    y: float

    /// The length of the vector.
    fn len(self) -> float {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    fn plus(self, other: Self) -> Self {
        Vec2 { x: self.x + other.x, y: self.y + other.y }
    }

    fn zero() -> Self {
        Vec2 { x: 0.0, y: 0.0 }
    }
}

let v = Vec2 { x: 3.0, y: 4.0 }
print(v.len())
print(Vec2.zero().plus(v))
```

### Adding methods with `extend`

An `extend` block adds methods to a struct or enum. The block must be in the module that declares the type.

```sand
struct Vec2 {
    x: float
    y: float
}

extend Vec2 {
    fn scale(self, k: float) -> Vec2 {
        Vec2 { x: self.x * k, y: self.y * k }
    }
}

print(Vec2 { x: 1.0, y: 2.0 }.scale(3.0))
```

You cannot extend built-in types such as `str` or `[T]`. To add methods to them, implement one of your own traits for them (see [Traits and generics](/guide/traits/)).

## Structs are references

A struct value is a reference, like a list or a map. Assignment and function calls share the same value. They do not copy it.

```sand
struct Account {
    owner: str
    var balance: int
}

fn deposit(a: Account, amount: int) {
    a.balance += amount
}

let a = Account { owner: "ann", balance: 10 }
let b = a
deposit(b, 5)
print(a.balance)

let copy = Account { ..a }
copy.balance = 0
print(a.balance)
```

To make a copy, use `..base` in a new literal. The `==` operator compares structs by content, field by field.

## Enums

An enum lists its variants. A variant can have no payload, or payload fields that are named or positional.

```sand
import math

enum Shape {
    Circle(radius: float)
    Rect(w: float, h: float)
    Empty

    fn area(self) -> float {
        match self {
            Circle(r) => math.pi * r * r
            Rect(w, h) => w * h
            Empty => 0.0
        }
    }
}

let shapes = [Shape.Circle(1.0), Shape.Rect(w: 2.0, h: 3.0), Shape.Empty]
for s in shapes {
    print("\(s): \(s.area())")
}
```

Write a variant as `Enum.Variant` in expressions. You can give named payload fields as named arguments, as in `Shape.Rect(w: 2.0, h: 3.0)`. In a pattern, you can omit the enum name when Sand knows the type, and payloads are always positional. [Pattern matching](/guide/patterns/) shows all pattern forms.

Enums can have methods and `extend` blocks, as structs do. Enum values are immutable.

A variant with a payload is also a function value:

```sand
enum Token {
    Num(int)
    Op(str)
}

let tokens = [1, 2, 3].map(Token.Num)
print(tokens)
print(tokens + [Token.Op("+")])
```

`Option` and `Result` are enums from the prelude. [Errors](/guide/errors/) explains them.

## Generic types

Structs and enums can have type parameters. Sand infers the type arguments from the literal or the variant.

```sand
struct Pair<A, B> {
    first: A
    second: B

    fn swap(self) -> Pair<B, A> {
        Pair { first: self.second, second: self.first }
    }
}

enum Tree<T> {
    Leaf
    Node(left: Tree<T>, value: T, right: Tree<T>)

    fn size(self) -> int {
        match self {
            Leaf => 0
            Node(l, _, r) => l.size() + 1 + r.size()
        }
    }
}

print(Pair { first: 1, second: "one" }.swap())

let t = Tree.Node(Tree.Leaf, 5, Tree.Node(Tree.Leaf, 7, Tree.Leaf))
print(t.size())
```

[Traits and generics](/guide/traits/) shows how to put bounds on type parameters.

## Type aliases

A `type` declaration gives a new name to an existing type. The alias and the type are the same type, so you can use them interchangeably.

```sand
type Grid = [[bool]]
type Span<T> = (T, T)

fn blank(w: int, h: int) -> Grid {
    [[false; w]; h]
}

let g = blank(3, 2)
let rows: Span<int> = (0, g.len() - 1)
print(g)
print(rows)
```

An alias cannot refer to itself. To define a recursive type, use a struct or an enum.
