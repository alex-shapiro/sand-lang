# Tradeoffs

## Public paths follow the file layout

This branch gives each item one public path: the path of the module that
defines it, such as `sandlang::policy::rules::RulePolicy`. Thus the file layout
is the public API. If a type moves to another file, embedders must change their
imports. The README example now has three `use` lines in place of one. Most
Rust crates give a curated facade instead.

The rule has two other costs:

- The derive macros generate paths into five modules (`native`, `native::ty`,
  `native::convert`, `vm`, `vm::value`), so a module move also changes the
  macros crate.
- The crate does not re-export `jiff` and `indexmap`. Embedders must depend on
  the same versions, or they get errors such as "expected `jiff::Timestamp`,
  found `jiff::Timestamp`".

Options:

1. Keep the rule (this branch). A reader finds each definition from its path.
   Embedders have long imports, and a file move is a breaking change.
2. Keep one path per item, but let each parent module re-export the items of
   its private children, such as `policy::RulePolicy` and `files::MemFs`. The
   paths stay short and stable. A reader must follow one re-export to find a
   definition.
3. Move each definition into a module named for its purpose, such as
   `OutputSink` into `engine` and `MemFs` into `files`. This keeps the rule
   and makes the paths shorter, but it is a large move of code.

Options 2 and 3 also allow a hidden `__derive` module for the macros and the
`jiff` and `indexmap` re-exports, as exceptions that follow one stated rule.
