+++
title = "Errors"
description = "Handle missing values and failed operations in Sand with Option, Result, the ? operator and ??, and learn which failures end a run."
+++

Sand has no `null` and no exceptions. A value that can be absent has an `Option` type. An operation that can fail returns a `Result`. The type checker makes you handle both cases before you use the value. A small set of failures, such as a failed `assert`, end the run.

## Optional values

The type `T?` is short for `Option<T>`. Its value is `Some(x)` or `None`.

```sand
let names = ["ann", "bo"]
print(names.find(n => n.starts_with("b")))
print(names.find(n => n.starts_with("z")))
print("42".parse_int())
```

A `T?` is not a `T`. To use the value, you must first handle the `None` case:

```sand error
let i = [4, 5].find_index(x => x == 5)
print(i + 1)
```

Use `match`, `if let` or `let ... else` (see [Pattern matching](/guide/patterns/)), or one of the operators below. The [Option reference](/std/prelude/Option/) lists the methods, such as `map`, `filter` and `ok_or`.

## Results

`Result<T, E>` is `Ok(T)` or `Err(E)`. Most code uses `Result<T>`, which is short for `Result<T, Error>`.

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

The [Result reference](/std/prelude/Result/) lists the methods, such as `map`, `map_err` and `and_then`.

### The `Error` struct

`Error` is a prelude struct with two fields. `kind` is a short tag for programs to test. `message` is a description for people. The function `error(message, kind)` makes an `Error`. If you omit `kind`, its value is `"error"`.

```sand
let e = error("disk is full", kind: "io")
print(e)
print(e.kind)
```

The standard library uses these kinds: `"error"`, `"not_found"`, `"permission_denied"`, `"already_exists"`, `"io"`, `"timeout"`, `"parse"`, `"invalid"`, `"http"`, `"closed"` and `"limit"`. Use them in your own errors when they apply.

## Propagation with `?`

The `?` operator after an expression unwraps a `Some` or an `Ok`. On `None` or `Err`, it returns that value from the current function immediately.

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

The current function must return the same kind of value. Use `?` on an `Option` in a function that returns an `Option`. Use `?` on a `Result<T, E>` in a function that returns a `Result` with the same `E`. Otherwise the compiler shows an error:

```sand error
fn half(text: str) -> Result<int> {
    let n = text.parse_int()?
    Ok(n / 2)
}
```

To convert, use `ok_or` (from `Option` to `Result`), `ok()` (from `Result` to `Option`) or `map_err` (to change the error type).

## Default values with `??`

The `??` operator gives the payload of a `Some` or an `Ok`. On `None` or `Err`, it evaluates the expression on its right side. Sand evaluates the right side only when it needs it.

```sand
let ports = ["http": 80]
print(ports.get("https") ?? 443)
print("x".parse_int() ?? 0)
```

## Failures

Some errors are not values. They are failures, and a failure ends the run. A script cannot catch a failure, and [deferred code](/guide/resources/) does not run. The tool prints the kind, the message and a stack trace, and `sand run` exits with code 1.

These functions cause a failure:

- `fail(message, kind)` fails with the message. The default kind is `"error"`.
- `assert(cond, message)` fails (kind `"assert"`) if `cond` is false.
- `todo()` marks code that you did not write yet.
- `unwrap()` gives the payload of `Some` or `Ok`, and fails on `None` or `Err`. `expect(message)` does the same with your message.

```sand fails
let scores: [int] = []
let best = scores.max().expect("no scores yet")
print(best)
```

Faults that the program cannot recover from also fail the run. Examples are a list index out of range, a missing map key, integer overflow and division by zero.

Use a failure for a bug in the program, or for a condition from which the program cannot continue. For problems that the caller can handle, return a `Result`.

## `?` at the top level

At the top level of the main module, there is no function to return from. There, `?` unwraps the value, and on `Err` it ends the run with that error. On `None`, it ends the run with the kind `"none"`. This is not a failure: before the run ends, Sand runs the deferred code of the top level. Then `sand run` exits with code 1. This keeps short scripts short:

```sand fails
fn parse_age(text: str) -> Result<int> {
    text.parse_int().ok_or(error("not a number: \(text)", kind: "parse"))
}

let age = parse_age("31")?
print("age: \(age)")
let other = parse_age("old")?
print("not printed")
```

You can use top-level `?` only in the main module and in `test` blocks. In a module that other modules import, it is a compile error. There, return a `Result` from a function instead.

## Do not ignore results

If an expression statement gives a `Result` and the code does not use it, `sand lint` reports an `unused-result` warning. Handle the result with `?` or `match`, or discard it explicitly with `let _ =`.

```sand
import fs

let _ = fs.remove_file("cache.tmp")
fs.write_text("out.txt", "done")?
```
