# Lack of trait objects requires closed wrapper enums for heterogeneous collections

Sand does not support `dyn Trait` trait objects, associated types and
constants, or blanket implementations. To store multiple types that implement
the same trait in a single collection, a script must define a closed wrapper
enum with a variant for each concrete type and forward every trait method
manually.

Run:

```
$ sand run aspirational.sand
error: expected `]`, found identifier

$ sand run repro.sand
circle: 3.14159
rect: 6.0
```

Supporting `dyn Trait` allows `[dyn Shape]` to hold `Circle` and `Rect` values
and dispatch trait methods dynamically, as in `aspirational.sand`.
