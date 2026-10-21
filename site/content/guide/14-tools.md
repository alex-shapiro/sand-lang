+++
title = "Tools"
description = "Type-check, lint, format, test, REPL, and doc inspection."
+++

The `sand` CLI includes a type checker, linter, formatter, test runner, and interactive REPL.

## `sand check`

`sand check` parses and type-checks source files without executing them:

```sh
sand check main.sand
```

Diagnostics report the source span of each syntax or type error:

```sand error
let count: int = "five"
print(cout)
```

`sand run` runs the same checks before executing a program and aborts if any compile error is present.

## `sand lint`

`sand lint` type-checks a file and warns about suspicious or non-idiomatic constructs:

- `unused-variable`: a local variable is never read (variables prefixed with `_` are ignored).
- `unused-result`: a `Result` value is discarded without handling potential errors.
- `unnecessary-var`: a `var` binding is never reassigned and can be declared with `let`.
- `len-zero`: `xs.len() == 0` is used instead of `xs.is_empty()`.
- `unclosed-resource`: a file or network handle is never closed (see [Resources](/guide/resources/)).

```sand
fn area(w: int, h: int) -> int {
    let unused = w * 2
    w * h
}

let items = [1, 2, 3]
if items.len() == 0 {
    print("empty")
}
var total = area(2, 3)
print(total)
```

```text
$ sand lint main.sand
main.sand:2:9: warning[unused-variable]: unused variable `unused`
main.sand:7:4: warning[len-zero]: use `is_empty()` instead of comparing `len()` with 0
main.sand:10:5: warning[unnecessary-var]: `total` is never reassigned; use `let`
```

Suppress a lint on the following line with `// lint: allow(name)`, or across an entire file with `//! lint: allow(name)`:

```sand
// lint: allow(unused-variable)
let kept_for_later = [1, 2, 3]
```

For the full list of lint rules, see [§17.2 of the specification](/spec/#172-sand-lint-file).

## `sand fmt`

`sand fmt` formats `.sand` and `.policy` files to the canonical style (4-space indentation and a 100-character line width) while preserving comments:

```sh
sand fmt main.sand           # format the file in place
sand fmt --check main.sand   # exit with code 1 if the file is not formatted
```

## Tests

Declare unit and property tests inside `test` blocks alongside your code. The standard `test` module provides assertion helpers that format both operands on failure:

```sand
import test

fn reverse(s: str) -> str {
    s.chars().reversed().join()
}

test "reverse a word" {
    test.assert_eq(reverse("sand"), "dnas")
}

test "reverse twice gives the input" {
    test.property("round trip", rng => rng.int(0, 1000).to_str(), s => reverse(reverse(s)) == s)
}

test "a failing test" {
    test.assert_eq(reverse("ab"), "ab")
}
```

`sand run` skips `test` blocks. `sand test` runs each `test` block in an isolated task:

```text
$ sand test main.sand
ok reverse a word
ok reverse twice gives the input
FAIL a failing test: assert: left == right
  left: "ba"
 right: "ab"
2 passed, 1 failed
```

A test passes if its block finishes without failing. Each test starts from a snapshot of the top-level module variables captured after top-level initialization.

`test.property(name, gen, prop)` runs a deterministic property check: it invokes `gen` with a seeded random generator to produce 100 inputs and verifies that `prop(input)` returns `true` for each input. For all test assertions and generators, see the [`test` reference](/std/test/).

`sand test` accepts the same sandbox policy and limit flags as `sand run`.

## `sand repl`

`sand repl` runs an interactive read-eval-print loop. Each input can be a statement, declaration, or expression, and the REPL prints the value of a trailing expression:

```text
$ sand repl
> let xs = [3, 1, 2]
> xs.sorted()
[1, 2, 3]
> fn double(n: int) -> int {
...     n * 2
... }
> xs.map(double)
[6, 2, 4]
> :type double
fn(int) -> int
```

Bindings persist across inputs and can be shadowed by later declarations. Spawned background tasks continue running while the REPL waits for input. End an expression with `;` to suppress output.

| Command       | Action                                             |
| ------------- | -------------------------------------------------- |
| `:type EXPR`  | print the static type of `EXPR` without running it |
| `:test`       | run all `test` blocks defined in the session       |
| `:reset`      | clear all session bindings and state               |
| `:help`       | list REPL commands and key bindings                |
| `:quit`, `:q` | exit the session                                   |

## `help` and `members`

`help(x)` prints documentation for a module, type, function, method, or value, and `help()` prints an overview of the prelude and standard library:

```sand
help(str.split)
help(task.race)
```

`members(x)` returns a `[str]` of the member names accessible on `x`:

```sand
print(members(task))
```

Both functions are evaluated during type checking and do not execute their argument expressions at runtime.

## Exit codes

All `sand` subcommands use consistent exit codes:

| Code | Condition                                                                             |
| ---- | ------------------------------------------------------------------------------------- |
| 0    | success                                                                               |
| 1    | runtime error, lint warning, test failure, or unformatted file (`sand fmt --check`)   |
| 2    | compile error, unreadable file, or invalid CLI flag                                   |
| 3    | resource limit exceeded (see [Sandbox and policies](/guide/sandbox/#resource-limits)) |

Calling `exit(code)` from a script exits with `code`. Internal engine errors exit with code 70, and interrupted runs (Ctrl-C) exit with code 130.
