+++
title = "Modules"
description = "Split a Sand program into files, import standard and script modules, control visibility with pub, and read module documentation with help."
+++

Every Sand source file is a module. A module uses `import` to get access to another module, and it can use only the items that the other module marks `pub`. Sand finds a standard module by its name and a script module by its file path.

## Standard modules

Import a standard module by its name. Then refer to its items as `module.name`. Use `as` to give the module a different name. Use `from ... import` to bind some items directly.

```sand
import json
import math as m
from math import cbrt, pi

print(json.encode(["a": [1, 2]]))
print(m.hypot(3.0, 4.0))
print(cbrt(27.0))
print(pi)
```

Put the imports at the start of the file. The prelude (`print`, `Option`, `str` methods and more) is always in scope without an import. The [library reference](/std/) lists all the standard modules.

## Script modules

To import another file, give its path as a string. Sand finds the path relative to the directory of the module that has the `import`. The `.sand` extension is optional. Sand binds the module to the file name without its extension.

Here is a module in the file `geometry.sand`:

```sand file=geometry.sand
//! Shapes and their areas.

/// A rectangle with width `w` and height `h`.
pub struct Rect {
    w: float
    h: float

    /// The area of the rectangle.
    fn area(self) -> float {
        self.w * self.h
    }
}

/// The unit of area.
pub let UNIT = "m2"

/// A square with side `side`.
pub fn square(side: float) -> Rect {
    Rect { w: side, h: side }
}

fn is_valid(r: Rect) -> bool {
    r.w >= 0.0 and r.h >= 0.0
}
```

A file `main.sand` in the same directory can use it:

```sand
import "./geometry.sand"

let r = geometry.Rect { w: 2.0, h: 3.0 }
print("\(r.area()) \(geometry.UNIT)")
print(geometry.square(4.0).area())
```

The other import forms also work with paths:

```sand
import "./geometry" as geo
from "./geometry.sand" import Rect, square

print(geo.UNIT)
print(square(1.0) == Rect { w: 1.0, h: 1.0 })
```

## Visibility

Items are private to their module unless you mark them `pub`. You can mark functions, structs, enums, traits, type aliases and top-level `let` and `var` bindings. The fields and methods of a public type are public. A use of a private item is a compile error:

```sand error
import "./geometry.sand"

print(geometry.is_valid(geometry.square(1.0)))
```

Other modules can read a public `var`, but they cannot assign it. To let them change it, give them a public function that does the assignment.

## Top-level code

A module can have top-level statements. Sand runs the top-level code of each imported module once, before the code of the first module that imports it. If two modules import the same file, they share one copy of its top-level variables.

Import cycles are a compile error: a module cannot import itself, directly or through other modules.

In an imported module, top-level `?` is a compile error, because the module cannot end the run. Put fallible code in a function that returns a `Result`. The main module can use top-level `?` (see [Errors](/guide/errors/#at-the-top-level)).

## The module root and the policy

A script import must stay in the module root. By default, the module root is the directory of the main module. An import of a path outside it, such as `"../shared.sand"`, is a compile error.

The sandbox also checks each script import against the policy before Sand loads the file. If the policy denies the import, the compiler reports an error. By default, the `sand` tool permits all imports in the module root. An `allow import` rule limits imports to its paths, and a `deny import` rule denies its paths. For example, this policy file denies one module:

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

[The sandbox](/guide/sandbox/) explains policies. An application that embeds Sand can set a different module root and add its own native modules (see [Embedding](/guide/embedding/)).

## Documentation of a module

Write a `//!` comment at the start of a file to document the module. Write `///` comments before items to document them. The `help` function prints this documentation. It works for standard modules, even without an import, and for items in them:

```sand
help(math.hypot)
help(str.split)
```

For `help(geometry)`, Sand prints the module comment and a list of the public items, each with the first sentence of its documentation:

```sand
import "./geometry.sand"

help(geometry)
```

The function `members(module)` returns the names of the public items as a list of strings.
