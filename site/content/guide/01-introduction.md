+++
title = "Introduction"
description = "Overview, installation instructions, and running your first script."
+++

Sand is a statically typed scripting language designed to execute untrusted code inside a capability-based sandbox. A bytecode virtual machine written in safe Rust runs each script in-process, enforcing policy rules on files, network connections, environment variables, and standard streams, alongside hard limits on memory, execution time, and instruction count.

You can run Sand from the command line, embed it in a Rust application, or run it in a browser via WebAssembly.

> **Sand is alpha software.** The language, standard library, and embedding API change without notice. Do not use Sand in production yet.

## What Sand code looks like

```sand
struct Item {
    name: str
    price: float
    quantity: int
}

let cart = [
    Item(name: "pen", price: 1.25, quantity: 4),
    Item(name: "notebook", price: 3.5, quantity: 2),
]

let total = cart.map(i => i.price * i.quantity.to_float()).sum()
print("\(cart.len()) items, total \(total:.2)")
```

Function signatures and struct fields declare explicit types, while local types are inferred. Integer arithmetic is checked for overflow, optional values use `Option<T>` instead of null references, and `if`, `match`, and blocks are expressions.

## Install

Build and install the `sand` CLI from source with Cargo:

```sh
git clone https://github.com/alex-shapiro/sand-lang
cd sand-lang
cargo install --path cli
```

## Hello, world

Create a file named `hello.sand`:

```sand
print("Hello, world!")
```

Run the script with `sand run`:

```sh
sand run hello.sand
```

`sand run` prompts before a script accesses external files, network hosts, or environment variables. Pass rules such as `--allow='read ./data'` and `--allow='net api.example.com'` to grant capabilities in advance, and pass flags such as `--max-time=5s` to set resource limits. See [Sandbox and policies](/guide/sandbox/) for details.

## The REPL

`sand repl` starts an interactive session that type-checks and evaluates each input and prints the value of a trailing expression:

```text
> let xs = [1, 2, 3]
> xs.map(x => x * 2)
[2, 4, 6]
> :type xs.first()
int?
```

Call `help()` to list standard library modules, or `help(str)` to view documentation for a specific symbol. Enter `:help` to list REPL commands and `:quit` to exit.

## The playground

The browser [playground](/play/) runs the Sand engine via WebAssembly without local installation. Every executable example in this guide runs under the same sandbox configuration during the site build.

## Other tools

The `sand` CLI includes a type checker, linter, code formatter, and test runner:

```sh
sand check script.sand   # type-check without running
sand lint script.sand    # report lint warnings
sand fmt script.sand     # format in place
sand test script.sand    # run test blocks
```

See [Tools](/guide/tools/) for details.

## The chapters

1. [Introduction](/guide/introduction/): overview and installation.
2. [Basics](/guide/basics/): bindings, primitive types, arithmetic, and strings.
3. [Functions](/guide/functions/): parameters, lambdas, and closures.
4. [Control flow](/guide/control-flow/): blocks, `if`, `match`, and loops.
5. [Collections](/guide/collections/): lists, maps, sets, tuples, and iterators.
6. [Structs and enums](/guide/structs-and-enums/): user-defined types and methods.
7. [Pattern matching](/guide/patterns/): destructuring and exhaustiveness checking.
8. [Errors](/guide/errors/): `Option`, `Result`, and the `?` operator.
9. [Traits and generics](/guide/traits/): traits, implementations, and generic functions.
10. [Modules](/guide/modules/): imports and visibility.
11. [Concurrency](/guide/concurrency/): tasks, channels, and asynchronous I/O.
12. [Resources](/guide/resources/): resource cleanup with `use` and `defer`.
13. [Sandbox and policies](/guide/sandbox/): capability policies and resource limits.
14. [Tools](/guide/tools/): checker, linter, formatter, and test runner.
15. [Embedding](/guide/embedding/): embedding Sand in a Rust application.

For complete reference material, see the [language specification](/spec/) and the [standard library reference](/std/).
