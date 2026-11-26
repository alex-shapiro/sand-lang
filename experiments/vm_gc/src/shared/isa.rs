//! The bytecode instruction set and program layout that every arm runs.

/// A register index relative to the current frame base.
pub type Reg = u16;

/// One register-machine instruction. Every instruction costs one unit of
/// fuel, and instructions that scan text or map keys also cost one unit per
/// 16 bytes or entries.
///
/// Operands named `base..base + count` are in the argument area above a
/// frame's named registers, where the assembler copies them. An
/// instruction may move them out, so they hold no value afterward.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    /// `dst = consts[idx]`.
    Const {
        dst: Reg,
        idx: u16,
    },
    Int {
        dst: Reg,
        value: i32,
    },
    Unit {
        dst: Reg,
    },
    None {
        dst: Reg,
    },
    Bool {
        dst: Reg,
        value: bool,
    },
    /// `dst` = the payload-less variant with global tag `tag`.
    Tag {
        dst: Reg,
        tag: u16,
    },
    Move {
        dst: Reg,
        src: Reg,
    },

    Add {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Sub {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Mul {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Div {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Rem {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    FAdd {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    FSub {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    FMul {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    FDiv {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Sqrt {
        dst: Reg,
        src: Reg,
    },
    /// `dst = a << b`. Fails if `b` is not in `0..64`.
    Shl {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    /// Arithmetic `dst = a >> b`. Fails if `b` is not in `0..64`.
    Shr {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    BitAnd {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    BitXor {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    ToFloat {
        dst: Reg,
        src: Reg,
    },
    Lt {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Le {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    /// Equality of scalars and strings.
    Eq {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    Not {
        dst: Reg,
        src: Reg,
    },

    Jump {
        target: u32,
    },
    JumpIf {
        cond: Reg,
        target: u32,
    },
    JumpIfNot {
        cond: Reg,
        target: u32,
    },

    /// Calls `funcs[func]` with the `argc` arguments in registers `base..`.
    /// The callee frame starts at register `base`.
    Call {
        dst: Reg,
        func: u16,
        base: Reg,
        argc: u8,
    },
    /// Calls the closure in register `callee`.
    CallClosure {
        dst: Reg,
        callee: Reg,
        base: Reg,
        argc: u8,
    },
    /// Calls the implementation of `methods[method]` for the type of the
    /// receiver in register `base`.
    CallMethod {
        dst: Reg,
        method: u16,
        base: Reg,
        argc: u8,
    },
    /// Calls the linked native function `natives[native]`.
    CallNative {
        dst: Reg,
        native: u16,
        base: Reg,
        argc: u8,
    },
    Return {
        src: Reg,
    },

    /// Creates a closure over `funcs[func]` that captures registers `base..base + count`.
    Closure {
        dst: Reg,
        func: u16,
        base: Reg,
        count: u8,
    },
    /// Reads capture `idx` of the running closure.
    Capture {
        dst: Reg,
        idx: u16,
    },
    NewCell {
        dst: Reg,
        src: Reg,
    },
    CellGet {
        dst: Reg,
        cell: Reg,
    },
    CellSet {
        cell: Reg,
        src: Reg,
    },

    /// Creates a struct of type `structs[ty]` from registers `base..base + count`.
    Struct {
        dst: Reg,
        ty: u16,
        base: Reg,
        count: u8,
    },
    Field {
        dst: Reg,
        obj: Reg,
        idx: u16,
    },
    SetField {
        obj: Reg,
        idx: u16,
        src: Reg,
    },
    /// Creates the variant with global tag `tag` and payload registers `base..base + count`.
    Variant {
        dst: Reg,
        tag: u16,
        base: Reg,
        count: u8,
    },
    /// `dst` = the index of the variant in its enum declaration.
    VariantIndex {
        dst: Reg,
        src: Reg,
    },
    VariantField {
        dst: Reg,
        src: Reg,
        idx: u16,
    },
    IsNone {
        dst: Reg,
        src: Reg,
    },

    /// Creates a list from registers `base..base + count`.
    List {
        dst: Reg,
        base: Reg,
        count: u8,
    },
    /// Creates an empty list with the capacity in register `cap`.
    ListWithCapacity {
        dst: Reg,
        cap: Reg,
    },
    Push {
        list: Reg,
        src: Reg,
    },
    Pop {
        dst: Reg,
        list: Reg,
    },
    Index {
        dst: Reg,
        list: Reg,
        idx: Reg,
    },
    SetIndex {
        list: Reg,
        idx: Reg,
        src: Reg,
    },
    /// Length of a list, map, or string.
    Len {
        dst: Reg,
        src: Reg,
    },
    Map {
        dst: Reg,
    },
    MapSet {
        map: Reg,
        key: Reg,
        value: Reg,
    },
    /// `dst` = the value for `key`, or `None`.
    MapGet {
        dst: Reg,
        map: Reg,
        key: Reg,
    },
    /// `dst` = the key of entry `idx` in insertion order.
    MapKey {
        dst: Reg,
        map: Reg,
        idx: Reg,
    },
    /// `dst` = the value of entry `idx` in insertion order.
    MapValue {
        dst: Reg,
        map: Reg,
        idx: Reg,
    },

    Concat {
        dst: Reg,
        a: Reg,
        b: Reg,
    },
    IntToStr {
        dst: Reg,
        src: Reg,
    },
    /// `dst` = string `s` repeated `n` times.
    Repeat {
        dst: Reg,
        s: Reg,
        n: Reg,
    },
    Global {
        dst: Reg,
        idx: u16,
    },
    SetGlobal {
        idx: u16,
        src: Reg,
    },
}

const _: () = assert!(std::mem::size_of::<Op>() == 8);

/// A constant operand of [`Op::Const`].
#[derive(Clone, Debug, PartialEq)]
pub enum Const {
    Int(i64),
    Float(f64),
    Str(String),
}

/// A bytecode function.
#[derive(Clone, Debug)]
pub struct Func {
    pub name: String,
    /// Parameters occupy registers `0..params`.
    pub params: u16,
    /// Total register count of a frame.
    pub regs: u16,
    pub code: Vec<Op>,
}

/// A static value type that the codec walkers decode by.
#[derive(Clone, Debug, PartialEq)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    List(Box<Ty>),
    /// A map with string keys.
    Map(Box<Ty>),
    Option(Box<Ty>),
    Struct(u16),
    Enum(u16),
}

#[derive(Clone, Debug)]
pub struct FieldDef {
    pub name: String,
    pub ty: Ty,
    /// The field is a `var` field.
    pub var: bool,
}

#[derive(Clone, Debug)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<FieldDef>,
    /// The struct has at least one `var` field.
    pub mutable: bool,
}

#[derive(Clone, Debug)]
pub struct VariantDef {
    pub name: String,
    /// Payload field names, or `None` for a positional payload.
    pub field_names: Option<Vec<String>>,
    pub payload: Vec<Ty>,
    /// The global variant tag.
    pub tag: u16,
}

#[derive(Clone, Debug)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<VariantDef>,
}

/// The enum and declaration index of a global variant tag.
#[derive(Clone, Copy, Debug)]
pub struct TagInfo {
    pub enum_id: u16,
    pub index: u16,
}

/// The method name ids that the codec walkers dispatch on.
pub const ENCODE_METHOD: u16 = 0;
pub const DECODE_METHOD: u16 = 1;

/// An assembled program.
#[derive(Clone, Debug, Default)]
pub struct Program {
    pub funcs: Vec<Func>,
    pub consts: Vec<Const>,
    pub structs: Vec<StructDef>,
    pub enums: Vec<EnumDef>,
    pub tags: Vec<TagInfo>,
    /// Method names. Ids 0 and 1 are `encode` and `decode`.
    pub methods: Vec<String>,
    /// `struct_methods[ty][method]` is the implementing function.
    pub struct_methods: Vec<Vec<Option<u16>>>,
    /// `enum_methods[enum][method]` is the implementing function.
    pub enum_methods: Vec<Vec<Option<u16>>>,
    /// Names of the native functions that `CallNative` refers to.
    pub natives: Vec<String>,
    pub globals: Vec<String>,
    /// Types that `int` operands of codec natives refer to.
    pub types: Vec<Ty>,
}

impl Program {
    pub fn func(&self, name: &str) -> Option<u16> {
        self.funcs.iter().position(|f| f.name == name).map(|i| i as u16)
    }

    pub fn global(&self, name: &str) -> Option<u16> {
        self.globals.iter().position(|g| g == name).map(|i| i as u16)
    }

    pub fn struct_id(&self, name: &str) -> Option<u16> {
        self.structs.iter().position(|s| s.name == name).map(|i| i as u16)
    }

    pub fn enum_id(&self, name: &str) -> Option<u16> {
        self.enums.iter().position(|e| e.name == name).map(|i| i as u16)
    }

    /// The implementation of `method` for struct `ty`.
    #[inline(always)]
    pub fn struct_method(&self, ty: u16, method: u16) -> Option<u16> {
        self.struct_methods[ty as usize][method as usize]
    }

    /// The implementation of `method` for enum `id`.
    #[inline(always)]
    pub fn enum_method(&self, id: u16, method: u16) -> Option<u16> {
        self.enum_methods[id as usize][method as usize]
    }

    /// Checks that every native call passes the argument count that
    /// `arities[native]` requires.
    pub fn check_native_arity(&self, arities: &[u8]) -> Result<(), String> {
        for func in &self.funcs {
            for op in &func.code {
                if let Op::CallNative { native, argc, .. } = op
                    && arities.get(*native as usize) != Some(argc)
                {
                    let name = &self.natives[*native as usize];
                    return Err(format!("{}: `{name}` takes {:?} arguments, found {argc}", func.name, arities.get(*native as usize)));
                }
            }
        }
        Ok(())
    }

    /// The variant definition of global tag `tag`.
    #[inline]
    pub fn variant(&self, tag: u16) -> &VariantDef {
        let info = self.tags[tag as usize];
        &self.enums[info.enum_id as usize].variants[info.index as usize]
    }
}
