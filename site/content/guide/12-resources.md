+++
title = "Resources"
description = "Run cleanup code with defer, and close files and connections with use and the Close trait."
+++

Some values hold something outside the script, such as an open file or a network connection. You must close these values when you no longer need them. Sand gives you two statements for this: `defer` runs code when a block exits, and `use` closes a value when a block exits.

## `defer`

`defer e` runs the expression `e` when the enclosing block exits:

```sand
fn greet() {
    defer print("goodbye")
    print("hello")
}

greet()
```

The deferred code runs on every exit from the block: at the end, at `return`, at `break` or `continue`, and when `?` returns an error. Thus you write the cleanup once, next to the code that needs it:

```sand
fn parse_all(items: [str]) -> Result<int> {
    defer print("done")
    var sum = 0
    for item in items {
        sum += item.parse_int().ok_or(error("not a number: \(item)"))?
    }
    Ok(sum)
}

print(parse_all(["1", "2"]))
print(parse_all(["1", "x"]))
```

### Order

If a block has more than one `defer`, they run in reverse order: the last one runs first. Thus the cleanup undoes the setup steps in the opposite order.

```sand
for i in 1..3 {
    defer print("end \(i)")
    defer print("cleanup \(i)")
    print("start \(i)")
}
```

A `defer` in a loop body runs at the end of each iteration. A `defer` that the program does not reach does not run.

### Variables

The deferred code reads variables when it runs, not when Sand reaches the `defer` statement:

```sand
var status = "starting"
defer print("status: \(status)")
status = "finished"
```

At the top level of a module, deferred code runs when the top-level code finishes.

### Rules

The deferred expression must have type `()`. To discard a value, write `defer { let _ = f() }`. The deferred code cannot leave its own block, so `return`, `?`, and `break` or `continue` to an outer loop are errors:

```sand error
import fs

fn save() -> Result<()> {
    defer fs.write_text("log.txt", "saved")?
    Ok(())
}
```

When a run fails with an error (for example from `fail`) or calls `exit`, Sand stops at once and does not run the deferred code. A top-level `?` is different: it runs the deferred code of the top level before the run stops.

## `use` and `Close`

A resource is a value whose type implements the `Close` trait. `fs.File`, `net.Conn`, `http.Body` and the other handles of the standard library are resources. `use` binds a resource and closes it when the block exits:

```sand
import fs

fn first_line(path: str) -> Result<str?> {
    use f = fs.open(path)?
    f.read_line()
}

fs.write_text("todo.txt", "water the plants\nfeed the cat\n")?
print(first_line("todo.txt")?)
```

`use f = e` is the same as `let f = e` followed by `defer Close.close(f)`. Thus `use` and `defer` follow the same rules: they run on every exit, the last one first.

The value must implement `Close`. If an expression returns a `Result`, use `?` to get the resource first. To keep a value only so that Sand closes it, write `use _ = e`.

### Your own resources

Implement `Close` for a type to make it a resource. If a type holds a resource, implement `Close` for the type and close that resource in it.

```sand
struct Lock {
    name: str
}

impl Close for Lock {
    fn close(self) {
        print("release \(self.name)")
    }
}

fn acquire(name: str) -> Lock {
    print("acquire \(name)")
    Lock { name }
}

fn work() {
    use _ = acquire("a")
    use _ = acquire("b")
    print("working")
}

work()
```

Closing a resource a second time has no effect. After a resource is closed, its other operations fail with an `Err` of kind `closed`.

If a script does not close a resource, the garbage collector closes it when no value refers to it. Do not depend on this: you cannot know when the collector runs.

## Resource lints

`sand lint` follows each resource through a function and warns you about three mistakes:

- `unclosed-resource`: you create a resource and never close it or give it to other code.
- `resource-escapes`: a resource bound with `use` stays in use after its block exits, so the code that uses it later gets a closed resource.
- `resource-field`: a struct or enum holds a resource, but the type does not implement `Close`.

This program makes all three mistakes:

```sand
import fs

struct Log {
    file: fs.File
}

fn open_log(path: str) -> Result<fs.File> {
    use f = fs.open(path, fs.Mode.Append)?
    Ok(f)
}

fn size(path: str) -> Result<int> {
    let f = fs.open(path)?
    Ok(f.read_all()?.len())
}

fs.write_text("app.log", "started\n")?
print(size("app.log")?)
let log = Log { file: open_log("app.log")? }
print(log.file.write_str("more\n"))
```

The program runs, but the write fails because `open_log` closed the file. `sand lint` reports:

```text
main.sand:4:5: warning[resource-field]: field `file` holds a resource (File), but `Log` does not implement `Close`; implement it (closing `file`) so that `use` can close a `Log`
main.sand:9:5: warning[resource-escapes]: resource `f` is returned, but it is bound with `use`, which closes it when its block ends
main.sand:13:9: warning[unclosed-resource]: `f` holds a resource (File) that is never closed; bind it with `use f = ...` to close it when its block ends
main.sand:19:23: warning[unclosed-resource]: this resource (File) is never closed; bind it with `use` to close it when the block ends
```

This version has no warnings. `open_log` gives its file to the caller, `size` closes its file, `Log` closes the file that it holds, and `use` closes the `Log`:

```sand
import fs

struct Log {
    file: fs.File
}

impl Close for Log {
    fn close(self) {
        self.file.close()
    }
}

fn open_log(path: str) -> Result<fs.File> {
    let f = fs.open(path, fs.Mode.Append)?
    Ok(f)
}

fn size(path: str) -> Result<int> {
    use f = fs.open(path)?
    Ok(f.read_all()?.len())
}

fs.write_text("app.log", "started\n")?
print(size("app.log")?)
use log = Log { file: open_log("app.log")? }
print(log.file.write_str("more\n"))
```

If a resource must stay open after its block, write `// lint: allow(resource-escapes)` on the line before the warning. For more on lints, see [Tools](/guide/tools/).
