+++
title = "Collections"
description = "Lists, maps, sets and tuples in Sand, their common methods, comprehensions and lazy iterators."
+++

Sand has four built-in collection types: lists `List<T>` (written `[T]`), maps `Map<K, V>` (written `[K: V]`), sets `Set<T>` and tuples `(A, B)`.
Lists, maps and sets are mutable and have reference semantics. Tuples are immutable values.

## Lists

A list holds values of one type, in order. Indices start at 0:

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

`xs[i]` stops the script with an error if `i` is out of range. `xs.get(i)` returns an option instead: `Some(x)` or `None`.

```sand fails
let primes = [2, 3, 5]
print(primes[3])
```

`[value; n]` makes a list of `n` elements. Sand evaluates `value` once for each element, so `[[0; 3]; 2]` makes two separate rows.

An empty list needs a type annotation if the checker cannot infer the element type from later use:

```sand
let names: [str] = []
print(names.is_empty())
```

### Slices

`xs[a..b]` makes a new list from index `a` up to, but not including, `b`. You can omit either bound:

```sand
let letters = ["a", "b", "c", "d", "e"]
print(letters[1..3])
print(letters[..2])
print(letters[3..])
print(letters[1..=3])
```

Strings and bytes have slices too, but their indices are byte offsets. You cannot index a single character of a string with `s[i]`.

## Maps

A map associates keys with values. It keeps its entries in insertion order. `[:]` is the empty map:

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

`m[k]` stops the script with an error if the key is absent. `get` returns an option, and `get_or` returns a default value.
This example counts words:

```sand
let counts: [str: int] = [:]
for word in "the cat saw the dog".split(" ") {
    counts[word] = counts.get_or(word, 0) + 1
}
print(counts)
```

Map keys and set elements must be hashable. `int`, `str`, `bool`, `bytes` and `duration` are hashable, and so are tuples, enums and structs without `var` fields that contain only hashable values.
A `float` or a list is not hashable.

## Sets

A set holds unique values in insertion order. Make one with `set`:

```sand
let tags = set(["red", "blue"])
print(tags.add("red"))
print(tags.add("green"))
print(tags.contains("blue"))
print(tags.union(set(["blue", "pink"])))
print(tags.intersection(set(["blue", "pink"])))
```

`add` returns `true` if the value was not in the set yet.

## Tuples

A tuple groups a fixed number of values, which can have different types.
Read an element with `.0`, `.1` and so on, or destructure the tuple with `let`:

```sand
let point = (3, "north", true)
print(point.1)

let (distance, direction, _) = point
print("\(distance) steps \(direction)")
```

Tuples are immutable. Functions often return a tuple to give back more than one value.

## Reference semantics

A list, map or set is a reference to its contents. Assignment and function calls share the same contents; they do not copy them.
`let` makes the binding immutable, not the contents:

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

Use `copy()` to get an independent shallow copy. `==` compares collections by content.

## Comprehensions

A comprehension builds a list or a map from a loop, with an optional `if` filter:

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

Many `for` clauses work as nested loops, from left to right.

## Common methods

Lists have methods that take functions. Each call makes a new list or value and leaves the original unchanged:

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

`sorted`, `reversed` and `sorted_by` return a new list. `sort`, `reverse` and `sort_by` change the list in place.
For a list of numbers, `sum`, `product`, `min` and `max` work without a function:

```sand
let scores = [72, 95, 88]
print(scores.sum())
print(scores.max())
print(scores.zip(["ann", "bob", "cy"]))
```

Ranges have many of the same methods: `(1..=10).filter(n => n % 3 == 0)` works.
The [prelude reference](/std/prelude/) lists the methods of every collection type.

## Lazy iterators

The methods above are eager. `xs.map(f)` calls `f` on every element and builds the full result at once.
A lazy iterator `Iter<T>` does no work until you ask for an element. It builds no intermediate lists, it can stop early, and it can read from an endless source.

Call `iter()` on a list, map, set or range to get an iterator. Then chain adapters such as `map`, `filter`, `take` and `enumerate`:

```sand
let firsts = Iter.count_from(1)
    .map(n => n * n)
    .filter(n => n % 3 == 0)
    .take(3)
print(firsts.to_list())

let total = [1, 2, 3, 4].iter().map(x => x * x).sum()
print(total)
```

`Iter.count_from(1)` counts forever, but `take(3)` asks for three elements only. Thus the pipeline does a finite amount of work.

Adapters return a new iterator and run no function. Consumers such as `to_list`, `sum`, `count`, `find`, `first` and `join` pull the elements. A `for` loop is a consumer too:

```sand
let log = "header\nfirst\nsecond"
for (i, line) in log.lines().iter().enumerate().skip(1) {
    print("\(i): \(line)")
}
```

An iterator gives each element once. Elements that a consumer takes are gone:

```sand
let numbers = Iter.count_from(10, step: 5)
print(numbers.take(3).to_list())
print(numbers.next())
```

Other sources are `Iter.repeat(x)`, `Iter.once(x)`, `Iter.from_fn(f)` and `Iter.successors(first, f)`:

```sand
let powers = Iter.successors(Some(1), n => if n < 500 { Some(n * 3) } else { None })
print(powers.to_list())
print(Iter.repeat("ab").take(3).join("-"))
```

Use a lazy iterator for long or endless sequences, or when you stop early. For short lists, the eager methods are simpler.
Your own types can be iterators too: the [Traits and generics](/guide/traits/) chapter shows how to implement `Iterator<T>`.
