+++
title = "Tools"
description = "Check, lint, format and test Sand programs, explore code in the REPL, and read documentation with help()."
+++

The `sand` command contains all the tools for Sand: a type checker, a linter, a formatter, a test runner and a REPL.

## `sand check`

`sand check` type checks files without running them:

```sh
sand check main.sand
```

Each error shows the line and the position of the problem:

```sand error
let count: int = "five"
print(cout)
```

`sand run` does the same checks before it runs a program. Sand does not run a program that has type errors.

## `sand lint`

`sand lint` type checks a file, then reports code that is correct but probably wrong or unclear. For example, these lints look for common mistakes:

- `unused-variable`: a variable that the code never reads. Names that start with `_` are not reported.
- `unused-result`: a `Result` that the code discards, which can hide an error.
- `unnecessary-var`: a `var` that the code never assigns again. Use `let`.
- `len-zero`: `xs.len() == 0`. Use `xs.is_empty()`.
- `unclosed-resource`: a file or connection that the code never closes (see [Resources](/guide/resources/)).

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

To suppress a lint for the next line, write `// lint: allow(name)`. To suppress it for the whole file, write `//! lint: allow(name)`.

```sand
// lint: allow(unused-variable)
let kept_for_later = [1, 2, 3]
```

[§17.2 of the specification](https://github.com/alex-shapiro/sand-lang/blob/main/docs/SPEC.md#172-sand-lint-file) lists every lint.

## `sand fmt`

`sand fmt` rewrites files in the one standard style of Sand. It uses 4-space indentation and a line width of 100 characters, and it keeps your comments.

```sh
sand fmt main.sand           # format the file in place
sand fmt --check main.sand   # exit code 1 if the file is not formatted
```

The formatter has no style options. Thus all Sand code has the same style.

## Tests

A `test` block contains a test. Put tests in the same file as the code that they test. The `test` module has assertions that show both values when they fail:

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

`sand run` ignores `test` blocks. `sand test` runs each test in its own task:

```text
$ sand test main.sand
ok reverse a word
ok reverse twice gives the input
FAIL a failing test: assert: left == right
  left: "ba"
 right: "ab"
2 passed, 1 failed
```

A test passes if it finishes without an error. A failure in one test does not stop the other tests. Each test starts with the top-level state as the top-level code made it.

`test.property(name, gen, prop)` is a property test. It calls `gen` to make 100 random inputs and checks that `prop` is true for each one. The random seed is fixed, so a failing property fails again with the same input. For all assertions, see the [`test` reference](/std/test/).

`sand test` uses the same policy flags as `sand run`. A test that needs a file must have permission to read it.

## `sand repl`

`sand repl` starts an interactive session. Type a statement, a declaration or an expression. The REPL runs it once and shows the value of a final expression:

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

Later inputs can use the names of earlier inputs. A later input can define a name again, and the new definition replaces the old one. Tasks that an input starts keep running while the REPL waits for input. End an input with `;` to hide its value.

The REPL has these commands:

| Command       | Effect                                                 |
| ------------- | ------------------------------------------------------ |
| `:type EXPR`  | show the type of an expression without running it     |
| `:test`       | run the `test` blocks entered so far                   |
| `:reset`      | forget all inputs                                      |
| `:help`       | show the commands and the keys of the line editor      |
| `:quit`, `:q` | end the session                                        |

The REPL takes the policy and limit flags of `sand run`. Ctrl-C interrupts the running input, and Ctrl-D ends the session.

## `help` and `members`

`help(x)` prints the documentation of `x`. `x` can be a module, a type, a function, a method or any expression. `help()` with no argument gives an overview of the prelude and the standard modules.

```sand
help(str.split)
help(task.race)
```

`members(x)` returns the names that can follow `x.`:

```sand
print(members(task))
```

Both functions work in programs and in the REPL. The type checker evaluates them, so they never run the expression that you give them.

## Exit codes

All tools use the same exit codes:

| Code | Meaning                                                                         |
| ---- | ------------------------------------------------------------------------------- |
| 0    | success                                                                         |
| 1    | a runtime error, a lint warning, a failed test, or a file that is not formatted |
| 2    | a compile error, a file that Sand cannot read, or an incorrect command          |
| 3    | a resource limit (see [Sandbox and policies](/guide/sandbox/))                  |

A script can also stop with its own code: `exit(4)` makes `sand run` exit with code 4. An internal error of the engine gives code 70, and an interrupted run gives code 130.
