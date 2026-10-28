+++
title = "Custom types"
description = "Structs and enums."
+++

Sand provides two constructs for defining custom data types: `struct` groups named fields into a record, and `enum` defines a sum type with a closed set of variants.

## Structs

A `struct` declaration lists its fields and their types. Construct an instance with `Type(...)`, passing fields by name or in declaration order:

```sand
struct Point {
    x: float
    y: float
}

let p = Point(x: 3.0, y: 4.0)
print(p.x)
print(p)
```

### Mutable fields

Struct fields are immutable by default. Prefix a field declaration with `var` to allow mutation:

```sand
struct Counter {
    name: str
    var count: int = 0
}

let clicks = Counter(name: "clicks")
clicks.count += 1
clicks.count += 1
print(clicks)
```

Field mutability is independent of whether the struct reference is held in a `let` or `var` binding. Assigning to an immutable field is a compile error:

```sand error
struct Point {
    x: float
    y: float
}

let p = Point(x: 3.0, y: 4.0)
p.x = 0.0
```

### Positional fields and copies

Positional arguments populate fields in declaration order, and a trailing `..base` argument copies any remaining fields from `base`:

```sand
struct Point {
    x: float
    y: float
}

let x = 1.0
let p = Point(x, y: 2.0)
let q = Point(y: 9.0, ..p)
print(q)
```

Used as a value, a struct name is a constructor function that takes all fields in declaration order:

```sand
struct Tag {
    name: str
}

print(["new", "sale"].map(Tag))
```

## Methods

Methods can be declared directly inside a `struct` or `enum` body. Functions taking `self` as their first parameter are instance methods (`v.len()`), and functions without `self` are static functions (`Vec2.zero()`):

```sand
struct Vec2 {
    x: float
    y: float

    fn len(self) -> float {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    fn plus(self, other: Self) -> Self {
        Vec2(x: self.x + other.x, y: self.y + other.y)
    }

    fn zero() -> Self {
        Vec2(x: 0.0, y: 0.0)
    }
}

let v = Vec2(x: 3.0, y: 4.0)
print(v.len())
print(Vec2.zero().plus(v))
```

### Validating construction with `init`

A static `init` method replaces field-by-field construction so that `T(...)` calls `init`. Inside `init`, `Self(...)` constructs the struct field by field. If `init` returns a `Result`, the caller must handle the error:

```sand
struct Port {
    n: int

    fn init(n: int) -> Result<Port> {
        if n < 1 or n > 65535 {
            return Err(Error("no such port: \(n)", kind: "invalid"))
        }
        Ok(Self(n: n))
    }
}

print(Port(8080))
print(Port(0))
```

Standard library types use the same construction syntax: `Set([1, 2])`, `Channel(capacity: 8)`, `regex.Regex(r"\d+")?`, and `time.PlainDate(2024, 2, 29)?`.

### Adding methods with `extend`

An `extend` block attaches additional methods to a struct or enum declared in the same module:

```sand
struct Vec2 {
    x: float
    y: float
}

extend Vec2 {
    fn scale(self, k: float) -> Vec2 {
        Vec2(x: self.x * k, y: self.y * k)
    }
}

print(Vec2(x: 1.0, y: 2.0).scale(3.0))
```

Built-in types cannot be extended with `extend`; define and implement a trait instead (see [Traits and generics](/guide/traits/)).

## Structs are references

Structs use reference semantics: assigning a struct or passing it to a function shares the same instance. Use `Struct(..existing)` to allocate a shallow copy, and `==` to compare two structs field by field:

```sand
struct Account {
    owner: str
    var balance: int
}

fn deposit(a: Account, amount: int) {
    a.balance += amount
}

let a = Account(owner: "ann", balance: 10)
let b = a
deposit(b, 5)
print(a.balance)

let copy = Account(..a)
copy.balance = 0
print(a.balance)
```

## Enums

An `enum` declaration defines a set of variants, each of which may have positional or named payload fields:

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

Construct an enum value with `Enum.Variant(...)` (using positional or named arguments) and destructure it with `match` or `if let` (see [Pattern matching](/guide/patterns/)). Enum values are immutable.

A variant with payload fields can also be used as a constructor function value:

```sand
enum Token {
    Num(int)
    Op(str)
}

let tokens = [1, 2, 3].map(Token.Num)
print(tokens)
print(tokens + [Token.Op("+")])
```

## Generic types

Structs and enums can declare type parameters in `<...>`, which the compiler infers at construction sites:

```sand
struct Pair<A, B> {
    first: A
    second: B

    fn swap(self) -> Pair<B, A> {
        Pair(first: self.second, second: self.first)
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

print(Pair(first: 1, second: "one").swap())

let t = Tree.Node(Tree.Leaf, 5, Tree.Node(Tree.Leaf, 7, Tree.Leaf))
print(t.size())
```

## Type aliases

A `type` declaration defines a transparent alias for an existing type:

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

Type aliases cannot be recursive; use a `struct` or `enum` to define a recursive type.
