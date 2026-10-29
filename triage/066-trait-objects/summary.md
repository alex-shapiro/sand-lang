# I cannot keep different shapes in one list, even when they all implement `Shape`

TODO item 066 (Language): "Support trait objects (`dyn Trait`), associated types and constants…"

**Pain.** The first thing a Rust or Swift user writes for plugins, shapes, or handlers is `[dyn Shape]` (or `[any Shape]`). Sand does not parse `dyn`, and `[Shape]` fails with a hint (`<T: Shape>`) that cannot hold mixed types. The only path is a wrapper enum with one variant per type and a hand-written `match` in each method. Each new shape adds one variant and one arm per method. A script cannot accept types that a different module declares without editing that enum.

**Why it matters.** Heterogeneous collections are a basic idiom. The workaround doubles the code, and it is closed to extension. Blanket impls (`impl<T> Plugin for T`) and associated types/constants (`type Unit`, `const SIDES: int`) also fail, with parse errors that do not name the missing feature.

**Reproduce.**
```
sand run aspirational.sand
error: expected `]`, found identifier          (at `dyn Shape`)
# with `[Shape]` in place of `[dyn Shape]`:
error: `Shape` is a trait, not a type; use a type parameter that implements it (`<T: Shape>`)
error: type mismatch: expected Circle, found Rect; ... use a struct or a tuple for mixed values
sand run repro.sand          # enum workaround
circle: 3.14159
rect: 6.0
```

**Aspirational.** `aspirational.sand`: `[dyn Shape]` holds a `Circle` and a `Rect`, and each call dispatches through the value's own `impl`. At minimum, `dyn` must give an error that names the enum workaround (§4.9 already documents it).
