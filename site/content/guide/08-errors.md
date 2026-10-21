+++
title = "Errors"
description = "Option, Result, and unrecoverable faults."
+++

Sand represents absent values with `Option<T>` (`T?`) and recoverable errors with `Result<T, E>`, rather than null references or exceptions. Unrecoverable faults and failed assertions immediately terminate execution.

## Optional values

The type `T?` is shorthand for `Option<T>`, which is either `Some(x)` or `None`:

```sand
let names = ["ann", "bo"]
print(names.find(n => n.starts_with("b")))
print(names.find(n => n.starts_with("z")))
print("42".parse_int())
```

Because `T?` and `T` are distinct types, you must unwrap an optional value before using it as `T`:

```sand error
let i = [4, 5].find_index(x => x == 5)
print(i + 1)
```

Unwrap an `Option<T>` with pattern matching (`match`, `if let`, `let ... else`), the `?` or `??` operators, or methods on [`Option`](/std/prelude/Option/) such as `map`, `filter`, and `ok_or`.

## Results

`Result<T, E>` is either `Ok(T)` or `Err(E)`. Omitting the second type parameter (`Result<T>`) defaults the error type to the prelude `Error` struct:

```sand
fn parse_age(text: str) -> Result<int> {
    let Some(n) = text.parse_int() else {
        return Err(error("not a number: \(text)", kind: "parse"))
    }
    if n < 0 {
        return Err(error("negative age: \(n)", kind: "invalid"))
    }
    Ok(n)
}

print(parse_age("31"))
match parse_age("-4") {
    Ok(n) => print(n)
    Err(e) => print("\(e.kind): \(e.message)")
}
```

See the [`Result` reference](/std/prelude/Result/) for transformation methods such as `map`, `map_err`, and `and_then`.

### The `Error` struct

The prelude `Error` struct has two `str` fields: `kind` (a machine-readable category tag) and `message` (a human-readable description). Construct an `Error` with `error(message, kind: "error")`:

```sand
let e = error("disk is full", kind: "io")
print(e)
print(e.kind)
```

Standard library functions return errors with standard `kind` tags (`"error"`, `"not_found"`, `"permission_denied"`, `"already_exists"`, `"io"`, `"timeout"`, `"parse"`, `"invalid"`, `"http"`, `"closed"`, and `"limit"`).

## Propagation with `?`

The postfix `?` operator unwraps `Some(v)` or `Ok(v)`, or returns `None` or `Err(e)` early from the enclosing function:

```sand
fn total_age(a: str, b: str) -> Result<int> {
    Ok(parse_age(a)? + parse_age(b)?)
}

fn parse_age(text: str) -> Result<int> {
    text.parse_int().ok_or(error("not a number: \(text)", kind: "parse"))
}

print(total_age("30", "12"))
print(total_age("30", "x"))
```

The enclosing function must return a matching wrapper type (`Option` for `Option?`, or `Result<_, E>` for `Result<T, E>?`):

```sand error
fn half(text: str) -> Result<int> {
    let n = text.parse_int()?
    Ok(n / 2)
}
```

Use `Option.ok_or`, `Result.ok`, or `Result.map_err` to convert between wrapper or error types before applying `?`.

## Default values with `??`

The coalescing operator `??` returns the payload of `Some(v)` or `Ok(v)`, or lazily evaluates its right operand if the left operand is `None` or `Err(_)`:

```sand
let ports = ["http": 80]
print(ports.get("https") ?? 443)
print("x".parse_int() ?? 0)
```

## Failures

An unrecoverable failure immediately aborts execution without running pending `defer` or `use` cleanup blocks. You can trigger a failure explicitly with `fail(message, kind)`, `assert(cond, message)`, `todo()`, `unwrap()`, or `expect(message)`:

```sand fails
let scores: [int] = []
let best = scores.max().expect("missing scores")
print(best)
```

Runtime faults—such as out-of-bounds list indexing, missing map keys with `m[k]`, integer overflow, and division by zero—also abort execution. Use `Result` for recoverable errors and failures for invariant violations.

## `?` at the top level

At the top level of the entry module (or inside a `test` block), applying `?` to `Err(e)` or `None` runs top-level `defer` and `use` cleanup blocks and terminates the run with an error:

```sand fails
fn parse_age(text: str) -> Result<int> {
    text.parse_int().ok_or(error("not a number: \(text)", kind: "parse"))
}

let age = parse_age("31")?
print("age: \(age)")
let other = parse_age("old")?
print("not printed")
```

Top-level `?` is rejected at compile time inside imported library modules.

## Do not ignore results

If an expression statement produces an unhandled `Result`, `sand lint` emits an `unused-result` warning. Handle the result with `?` or `match`, or explicitly discard it with `let _ =`:

```sand
import fs

let _ = fs.remove_file("cache.tmp")
fs.write_text("out.txt", "done")?
```
