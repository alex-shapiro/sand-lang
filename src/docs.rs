//! Structured documentation of the prelude, the standard modules and host
//! modules, for tools that render reference pages ([`Engine::docs`]).
//!
//! The text is what `help` shows: declarations as Sand source and doc
//! comments as written (Markdown).
//!
//! [`Engine::docs`]: crate::Engine::docs

/// The documentation of a module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleDoc {
    /// `prelude`, or the name that scripts import.
    pub name: String,
    pub kind: ModuleKind,
    /// The module's doc comment (its `//!` lines).
    pub doc: Option<String>,
    /// The public items, in order of declaration. The prelude lists the
    /// built-in types (`int`, `[T]`, ...) first.
    pub items: Vec<ItemDoc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleKind {
    Prelude,
    Std,
    Host,
}

/// A public item of a module.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemDoc {
    pub kind: ItemKind,
    /// The name as scripts write it after the module's name: `Json`,
    /// `parse`, `pi`, and for built-in types `str`, `List<T>`, `Map<K, V>`.
    pub name: String,
    /// The declaration: `enum Json`, `fn parse(text: str) -> Result<Json, Error>`,
    /// `let pi: float`, `trait Reader`.
    pub decl: String,
    pub doc: Option<String>,
    /// A type's variants, fields and methods, or a trait's methods.
    pub members: Vec<MemberDoc>,
    /// The traits a type implements, or the types that implement a trait
    /// (`Iterator<T>`, `[T]`).
    pub related: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    /// A struct, enum, opaque, built-in type or type alias.
    Type,
    Trait,
    Function,
    /// A top-level `let` or `var`.
    Value,
}

/// A variant, field or method of a type or trait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberDoc {
    pub kind: MemberKind,
    pub name: String,
    /// `Circle(radius: float)`, `var count: int = 0`, `fn len(self) -> int`.
    pub decl: String,
    pub doc: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberKind {
    Variant,
    Field,
    Method,
}
