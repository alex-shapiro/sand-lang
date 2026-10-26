//! Structured documentation for the prelude, standard library modules, and
//! host modules (see [`Engine::docs`](crate::Engine::docs)).

/// Documentation for a module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleDoc {
    /// Module name (`"prelude"` for the prelude).
    pub name: String,
    pub kind: ModuleKind,
    /// Module-level `//!` Markdown documentation.
    pub doc: Option<String>,
    /// Public items in declaration order. In the prelude, built-in types
    /// (`int`, `List<T>`, ...) precede declared items.
    pub items: Vec<ItemDoc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModuleKind {
    Prelude,
    Std,
    Host,
}

/// Documentation for a public module item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemDoc {
    pub kind: ItemKind,
    /// Unqualified item name (`Json`, `parse`, `str`, `List<T>`).
    pub name: String,
    /// Formatted Sand declaration (`enum Json`, `fn parse(text: str) -> Result<Json, Error>`).
    pub decl: String,
    pub doc: Option<String>,
    /// The `init`, variants, fields, and methods of a type, or the methods of
    /// a trait.
    pub members: Vec<MemberDoc>,
    /// Implemented traits (if this item is a type) or implementing types (if
    /// this item is a trait).
    pub related: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// Built-in type (`int`, `str`, `List<T>`, `Range`).
    Builtin,
    /// Struct, enum, or opaque type.
    Type,
    Alias,
    Trait,
    Function,
    /// Top-level `let` or `var` binding.
    Value,
}

/// Documentation for a variant, field, method, or `init` of a type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberDoc {
    pub kind: MemberKind,
    pub name: String,
    /// Formatted Sand declaration (`Regex(pattern: str) -> Result<Regex>`, `Circle(radius: float)`, `var count: int = 0`, `fn len(self) -> int`).
    pub decl: String,
    pub doc: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    /// The type's `init`, shown as `T(params) -> T`.
    Construct,
    Variant,
    Field,
    Method,
}
