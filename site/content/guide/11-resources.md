+++
title = "Resources"
description = "Clean up code and close system resources safely."
+++

Resources such as open files and network sockets hold state outside the script and must be closed after use. Sand provides two statements for deterministic cleanup: `defer` runs an expression when the enclosing block exits, and `use` binds a value that implements `Close` and closes it when the enclosing block exits.

## `defer`

`defer expr` schedules `expr` to run when the enclosing block exits:

```sand
fn greet() {
    defer print("goodbye")
    print("hello")
}

greet()
```

Deferred expressions run on every block exit: normal completion, `return`, `break`, `continue`, and early return via `?`.

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

### Execution order

Multiple `defer` statements in the same block run in reverse registration order (last-in, first-out), so cleanup runs in the opposite order of acquisition.

```sand
for i in 1..3 {
    defer print("end \(i)")
    defer print("cleanup \(i)")
    print("start \(i)")
}
```

A `defer` inside a loop body runs at the end of each iteration. A `defer` statement takes effect only after execution reaches it.

### Captured variables

A deferred expression reads variables when the block exits, not when the `defer` statement is registered:

```sand
var status = "starting"
defer print("status: \(status)")
status = "finished"
```

At the top level of a module, deferred expressions run when the module finishes top-level execution.

### Restrictions

A deferred expression must have type `()`. To ignore a return value, write `defer { let _ = f() }`. Control flow cannot jump out of a deferred expression: `return`, `?`, and `break` or `continue` targeting an outer loop are compile errors:

```sand error
import fs

fn save() -> Result<()> {
    defer fs.write_text("log.txt", "saved")?
    Ok(())
}
```

If a script aborts with `fail` or terminates via `exit(code)`, Sand stops execution immediately without running pending `defer` expressions. If top-level execution exits via `?`, Sand runs top-level `defer` expressions before terminating with the error.

## `use` and `Close`

A resource is any value whose type implements the prelude `Close` trait, including `fs.File`, `net.Conn`, and `http.Body`. The `use` statement binds a resource variable and closes it when the enclosing block exits:

```sand
import fs

fn first_line(path: str) -> Result<str?> {
    use f = fs.open(path)?
    f.read_line()
}

fs.write_text("todo.txt", "water the plants\nfeed the cat\n")?
print(first_line("todo.txt")?)
```

`use f = expr` is equivalent to `let f = expr` followed by `defer Close.close(f)`. If `expr` returns a `Result`, unwrap it with `?` before binding it with `use`. Write `use _ = expr` to close a resource at block exit without binding a variable name.

### Custom resources

Implement `Close` on a custom type to use it with `use`. If a struct or enum contains a resource field, implement `Close` on the outer type and close the field inside `close`:

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

Closing a standard library resource more than once is a no-op. Any other operation on a closed resource returns an `Err` with kind `"closed"`.

## Resource lints

`sand lint` tracks resources through each function and warns about three resource bugs:

- `unclosed-resource`: a resource is created and neither closed nor returned to the caller.
- `resource-escapes`: a resource bound with `use` is returned or stored outside its block, where it will already be closed.
- `resource-field`: a struct or enum stores a resource field, but the type does not implement `Close`.

The following program triggers all three warnings:

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

`open_log` closes `f` before returning it, so `log.file.write_str` fails at runtime. `sand lint` reports each bug:

```text
main.sand:4:5: warning[resource-field]: field `file` holds a resource (File), but `Log` does not implement `Close`; implement it (closing `file`) so that `use` can close a `Log`
main.sand:9:5: warning[resource-escapes]: resource `f` is returned, but it is bound with `use`, which closes it when its block ends
main.sand:13:9: warning[unclosed-resource]: `f` holds a resource (File) that is never closed; bind it with `use f = ...` to close it when its block ends
main.sand:19:23: warning[unclosed-resource]: this resource (File) is never closed; bind it with `use` to close it when the block ends
```

Fixing the ownership and `Close` implementation resolves all warnings:

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

To suppress a resource warning on a specific line, place `// lint: allow(resource-escapes)` above the line (see [Tools](/guide/tools/)).
