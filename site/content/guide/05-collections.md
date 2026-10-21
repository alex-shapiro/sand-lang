+++
title = "Collections"
description = "Lists, maps, sets, tuples, comprehensions, and lazy iterators."
+++

Sand provides four built-in collection types: lists, maps, sets, and tuples.

## Lists

A list stores an ordered sequence of elements of a single type, indexed from `0`:

```sand
let primes = [2, 3, 5, 7, 11]
print(primes[0])
print(primes.len())
print(primes.get(10))

primes.push(13)
primes[0] = 1
print(primes)
print(primes.pop())
```

Indexing with `xs[i]` terminates execution with an error if `i` is out of bounds. `xs.get(i)` returns `Some(x)` or `None`:

```sand fails
let primes = [2, 3, 5]
print(primes[3])
```

The repeat syntax `[value; n]` evaluates `value` once for each of the `n` elements. `[[0; 3]; 2]` allocates two distinct inner lists.

An empty list literal requires a type annotation if the element type cannot be inferred:

```sand
let names: [str] = []
print(names.is_empty())
```

### Slices

Slicing (`xs[a..b]` or `xs[a..=b]`) returns a new list containing the selected range of elements:

```sand
let letters = ["a", "b", "c", "d", "e"]
print(letters[1..3])
print(letters[..2])
print(letters[3..])
print(letters[1..=3])
```

Strings and `bytes` also support range slicing using byte offsets, though `s[i]` single-character indexing is not supported on `str`.

## Maps

A map associates unique keys with values in insertion order (`[:]` creates an empty map):

```sand
let stock = ["apple": 3, "pear": 5]
stock["fig"] = 2
stock["apple"] += 1
print(stock)
print(stock.get("kiwi"))
print(stock.get_or("kiwi", 0))
print(stock.contains("pear"))
print(stock.remove("pear"))
print(stock.keys())
```

`m[k]` terminates execution with an error if `k` is missing; use `get(k)` for `Option<V>` or `get_or(k, default)` for a fallback value:

```sand
let counts: [str: int] = [:]
for word in "the cat saw the dog".split(" ") {
    counts[word] = counts.get_or(word, 0) + 1
}
print(counts)
```

Map keys and set elements must be hashable (`int`, `str`, `bool`, `bytes`, `duration`, and tuples, enums, or immutable structs composed of hashable types).

## Sets

A `Set<T>` stores unique values in insertion order:

```sand
let tags = set(["red", "blue"])
print(tags.add("red"))
print(tags.add("green"))
print(tags.contains("blue"))
print(tags.union(set(["blue", "pink"])))
print(tags.intersection(set(["blue", "pink"])))
```

`add` returns `true` if the value was newly inserted.

## Tuples

A tuple is an immutable fixed-length sequence of values that may have different types. Access elements with `.0`, `.1`, or destructure the tuple with a pattern:

```sand
let point = (3, "north", true)
print(point.1)

let (distance, direction, _) = point
print("\(distance) steps \(direction)")
```

## Reference semantics

Lists, maps, and sets use reference semantics: assigning or passing a collection shares the underlying storage rather than copying it. A `let` binding prevents variable reassigment but does not prevent mutation of collection contents:

```sand
let a = [1, 2]
let b = a
b.push(3)
print(a)

let c = a.copy()
c.push(4)
print(a)
print(c)
print(a == [1, 2, 3])
```

Call `copy()` to create a shallow copy, and use `==` to compare collections structurally by element.

## Comprehensions

List and map comprehensions build a collection from a sequence of `for` clauses and optional `if` filters:

```sand
let squares = [x * x for x in 1..=5]
let even_squares = [x * x for x in 1..=10 if x % 2 == 0]
let pairs = [(n, s) for n in 1..3 for s in ["a", "b"]]
let lengths = [w: w.len() for w in ["fig", "pear"]]
print(squares)
print(even_squares)
print(pairs)
print(lengths)
```

## Common methods

Lists provide functional methods that transform its elements without modifying the original value:

```sand
let words = ["apple", "fig", "banana", "cherry", "date"]

print(words.filter(w => w.len() > 4).map(w => w.to_upper()))
print(words.fold(0, (total, w) => total + w.len()))
print(words.any(w => w.starts_with("b")))
print(words.find(w => w.len() == 3))
print(words.sorted_by(w => w.len()))
print(words.min_by(w => w.len()))
print(words.group_by(w => w.len() > 4))
print(words.enumerate().take(2))
print(words.join(", "))
```

Some functional methods have in-place alternatives: `sort`, `reverse`, and `sort_by` mutate a list in place. Numeric lists also provide `sum`, `product`, `min`, and `max`:

```sand
let scores = [72, 95, 88]
print(scores.sum())
print(scores.max())
print(scores.zip(["ann", "bob", "cy"]))
```

See the [prelude reference](/std/prelude/) for the complete list of collection methods.

## Lazy iterators

Unlike the functional list methods above, an iterator computes elements on demand without allocating intermediate lists. Call `iter()` on a collection or range, or construct a generator with `Iter.count_from`, `Iter.repeat`, `Iter.once`, `Iter.from_fn`, or `Iter.successors`:

```sand
let firsts = Iter.count_from(1)
    .map(n => n * n)
    .filter(n => n % 3 == 0)
    .take(3)
print(firsts.to_list())

let total = [1, 2, 3, 4].iter().map(x => x * x).sum()
print(total)
```

Adapters return a new `Iter<T>` without advancing. Consumers pull elements from the iterator:

```sand
let log = "header\nfirst\nsecond"
for (i, line) in log.lines().iter().enumerate().skip(1) {
    print("\(i): \(line)")
}
```

Advancing an iterator consumes its elements in place:

```sand
let numbers = Iter.count_from(10, step: 5)
print(numbers.take(3).to_list())
print(numbers.next())
```

```sand
let powers = Iter.successors(Some(1), n => if n < 500 { Some(n * 3) } else { None })
print(powers.to_list())
print(Iter.repeat("ab").take(3).join("-"))
```

Custom types can integrate with `for` loops and `Iter<T>` by implementing `Iterator<T>`. For more, see [Traits and generics](/guide/traits/)).
