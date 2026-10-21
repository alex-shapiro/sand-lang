+++
title = "Introduction"
description = "What Sand is, how to install it, and how to run your first script."
+++

Sand is a statically typed scripting language for code that you do not fully trust.
A bytecode virtual machine, written in safe Rust, runs each script in a sandbox.
The script cannot touch files, the network, the environment or the standard streams unless the host allows it.
The host also sets hard limits on memory, CPU time and instructions.

You can run Sand from the command line, embed it in a Rust program, or run it in a web page.

> **Sand is alpha software.** The language, the standard library and the embedding API can change without notice.
> Do not use Sand in production yet.

## What Sand code looks like

```sand
struct Item {
    name: str
    price: float
    quantity: int
}

let cart = [
    Item { name: "pen", price: 1.25, quantity: 4 },
    Item { name: "notebook", price: 3.5, quantity: 2 },
]

let total = cart.map(i => i.price * i.quantity.to_float()).sum()
print("\(cart.len()) items, total \(total:.2)")
```

Function signatures and struct fields declare their types, and the checker infers most other types.
Integer overflow stops the script, there is no null, and `if`, `match` and blocks are expressions.

## Install

Build the `sand` command from source with Cargo:

```sh
git clone https://github.com/alex-shapiro/sand-lang
cd sand-lang
cargo install --path cli
```

## Hello, world

Write this line to a file named `hello.sand`:

```sand
print("Hello, world!")
```

Run the file:

```sh
sand run hello.sand
```

`sand run` denies all access to the outside world by default.
Options such as `--allow-read=DIR` and `--allow-net=HOST` grant access, and options such as `--max-time=5s` set limits.
The [Sandbox and policies](/guide/sandbox/) chapter explains the options.

## The REPL

`sand repl` starts an interactive session.
Sand checks and runs each input when you enter it, and shows the value of a final expression:

```text
> let xs = [1, 2, 3]
> xs.map(x => x * 2)
[2, 4, 6]
> :type xs.first()
int?
```

Enter `help()` for an overview of the library, or `help(str)` for the documentation of one type.
Enter `:help` for the REPL's commands, and `:quit` to end the session.

## The playground

The [playground](/play/) runs Sand in your browser, with no install.
The Sand examples in this guide run in the same sandbox: the site's build runs each one and shows its output below it.

## Other tools

The `sand` command also includes the checker, the linter, the formatter and the test runner:

```sh
sand check script.sand   # type check
sand lint script.sand    # report dubious code
sand fmt script.sand     # format in place
sand test script.sand    # run the test blocks
```

The [Tools](/guide/tools/) chapter describes them.

## The chapters

1. [Introduction](/guide/introduction/): this page.
2. [Basics](/guide/basics/): bindings, primitive types, arithmetic and strings.
3. [Functions](/guide/functions/): parameters, lambdas, closures and pipes.
4. [Control flow](/guide/control-flow/): blocks, `if`, `match` and loops.
5. [Collections](/guide/collections/): lists, maps, sets, tuples and iterators.
6. [Structs and enums](/guide/structs-and-enums/): your own types and their methods.
7. [Pattern matching](/guide/patterns/): destructuring and exhaustive `match`.
8. [Errors](/guide/errors/): `Option`, `Result` and the `?` operator.
9. [Traits and generics](/guide/traits/): shared behavior and generic bounds.
10. [Modules](/guide/modules/): imports and visibility.
11. [Concurrency](/guide/concurrency/): tasks, channels and asynchronous I/O.
12. [Resources](/guide/resources/): files, connections, `use` and `defer`.
13. [Sandbox and policies](/guide/sandbox/): policies and resource limits.
14. [Tools](/guide/tools/): the checker, linter, formatter and test runner.
15. [Embedding](/guide/embedding/): Sand in a Rust program.

The [specification](/spec/) defines the language exactly. The [library reference](/std/) documents every module.
