+++
title = "Modules"
description = "Organize a program into modules and control visibility."
+++

Each Sand source file is a module. Use `import` to load another module and access the items that it marks `pub`. Standard library modules are imported by name, and script modules are imported by relative file path.

## Standard modules

Import a standard module by name and refer to its items as `module.item`. Use `as` to bind the module to an alias, or use `from ... import` to bring specific items into scope.

```sand
import json
import math as m
from math import cbrt, pi

print(json.encode(["a": [1, 2]]))
print(m.hypot(3.0, 4.0))
print(cbrt(27.0))
print(pi)
```

Place `import` declarations at the top of the file. The prelude (`print`, `Option`, `str` methods, and built-in traits) is always in scope. For the list of standard modules, see the [standard library reference](/std/).

## Script modules

Import another source file by passing its relative path as a string literal. Sand resolves the path relative to the directory of the importing file. The `.sand` extension is optional, and the module is bound to the file stem unless aliased with `as`.

Suppose `geometry.sand` contains:

```sand file=geometry.sand
//! Shapes and their areas.

/// A rectangle with width `w` and height `h`.
pub struct Rect {
    pub w: float
    pub h: float

    /// Returns the area of the rectangle.
    pub fn area(self) -> float {
        self.w * self.h
    }
}

/// The unit of area.
pub let UNIT = "m2"

/// Returns a square with side length `side`.
pub fn square(side: float) -> Rect {
    Rect { w: side, h: side }
}

fn is_valid(r: Rect) -> bool {
    r.w >= 0.0 and r.h >= 0.0
}
```

A `main.sand` file in the same directory can import `geometry.sand`:

```sand
import "./geometry.sand"

let r = geometry.Rect { w: 2.0, h: 3.0 }
print("\(r.area()) \(geometry.UNIT)")
print(geometry.square(4.0).area())
```

Aliases and selective imports also work with file paths:

```sand
import "./geometry" as geo
from "./geometry.sand" import Rect, square

print(geo.UNIT)
print(square(1.0) == Rect { w: 1.0, h: 1.0 })
```

## Visibility

Module items are private by default. Mark top-level functions, structs, enums, traits, type aliases, and `let` or `var` bindings with `pub` to export them, and mark struct fields and methods with `pub` to expose them to importing modules. Other modules cannot construct a struct that has a private field. Referencing a private item from another module is a compile error:

```sand error
import "./geometry.sand"

print(geometry.is_valid(geometry.square(1.0)))
```

Importing modules can read a `pub var` binding, but only the declaring module can assign to it. Provide a public function if callers need to update module state.

## Top-level initialization

Sand executes the top-level statements of each imported module a single time, before the first module that imports it runs. If multiple modules import the same file, they share the same module state.

Cyclical imports are rejected at compile time.

Top-level `?` is allowed only in the entry module (see [Errors](/guide/errors/#at-the-top-level)); in an imported module, place fallible initialization inside a function that returns a `Result`.

## Module root and import policy

All script imports must stay inside the module root, which defaults to the directory of the entry file. Importing a path outside the module root (such as `"../shared.sand"`) is a compile error.

The compiler also checks each script import against the sandbox policy before loading the file. By default, the `sand` CLI allows all imports within the module root. In a policy file, `allow import` restricts imports to matching paths, and `deny import` blocks specific paths:

```policy
deny import ./geometry.sand
```

```text
error: cannot import `./geometry.sand`: permission denied
  --> main.sand:1:8
  |
1 | import "./geometry.sand"
  |        ^^^^^^^^^^^^^^^^^
```

For details on policy files, see [Sandbox and policies](/guide/sandbox/). Host applications that embed Sand can configure a custom module root and register native Rust modules (see [Embedding](/guide/embedding/)).

## Module documentation

Document a module with `//!` comments at the top of the file, and document individual declarations with `///` comments. The built-in `help` function formats and prints these comments:

```sand
help(math.hypot)
```

Passing a module to `help` prints the module summary and lists its public items:

```sand
import "./geometry.sand"

help(geometry)
```

The built-in `members(module)` function returns the names of the public items in `module` as a `[str]`.
