//! A text assembler for the shared bytecode programs.
//!
//! A program is a list of declarations:
//!
//! ```text
//! struct Address { street: str, city: str, zip: str? }
//! struct Cell { var next: Cell? }
//! enum Status { Active, Suspended(reason: str), Banned(int, str) }
//! global doc
//! fn Address.encode(self, enc) {
//!     native _ enc.open_map enc
//!     ...
//!     ret self
//! }
//! ```
//!
//! A function named `Type.method` implements `method` for `Type`. Each line
//! of a function body is a label (`@loop`) or an instruction: a mnemonic
//! followed by operands. Register operands are names. The assembler gives
//! each new name the next free register, after the parameters. `_` names a
//! scratch register. Operands that list registers (call arguments, struct
//! fields, list items, and captures) are copied into an argument area above
//! the named registers. `#` starts a comment.

use super::isa::*;
use std::collections::HashMap;

/// Assembles `source` into a program.
pub fn assemble(source: &str) -> Result<Program, String> {
    let mut asm = Assembler::default();
    asm.program.methods = vec!["encode".into(), "decode".into()];
    let lines: Vec<(usize, String)> = source.lines().enumerate().map(|(i, l)| (i + 1, strip_comment(l).trim().to_string())).collect();
    let mut bodies = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let (line_no, line) = &lines[i];
        let at = |e: String| format!("line {line_no}: {e}");
        if line.is_empty() {
            i += 1;
        } else if line.starts_with("struct ") || line.starts_with("enum ") {
            let mut text = line.clone();
            while !text.contains('}') {
                i += 1;
                text.push(' ');
                text.push_str(&lines.get(i).ok_or_else(|| at("unclosed declaration".into()))?.1);
            }
            asm.declare_type(&text).map_err(at)?;
            i += 1;
        } else if let Some(name) = line.strip_prefix("global ") {
            asm.program.globals.push(name.trim().to_string());
            i += 1;
        } else if let Some(header) = line.strip_prefix("fn ") {
            let (name, params) = parse_fn_header(header).map_err(at)?;
            let start = i + 1;
            while lines.get(i).is_some_and(|(_, l)| l != "}") {
                i += 1;
            }
            if i >= lines.len() {
                return Err(at("unclosed function".into()));
            }
            asm.program.funcs.push(Func { name: name.clone(), params: params.len() as u16, regs: 0, code: Vec::new() });
            bodies.push((name, params, lines[start..i].to_vec()));
            i += 1;
        } else {
            return Err(at(format!("unexpected `{line}`")));
        }
    }
    asm.resolve_types()?;
    let struct_count = asm.program.structs.len();
    let enum_count = asm.program.enums.len();
    asm.program.struct_methods = vec![Vec::new(); struct_count];
    asm.program.enum_methods = vec![Vec::new(); enum_count];
    for (index, (name, _, _)) in bodies.iter().enumerate() {
        if let Some((ty, method)) = name.split_once('.') {
            let method = asm.method(method);
            if let Some(id) = asm.program.struct_id(ty) {
                set_method(&mut asm.program.struct_methods[id as usize], method, index as u16);
            } else if let Some(id) = asm.program.enum_id(ty) {
                set_method(&mut asm.program.enum_methods[id as usize], method, index as u16);
            } else {
                return Err(format!("method `{name}` of unknown type"));
            }
        }
    }
    for (index, (name, params, body)) in bodies.into_iter().enumerate() {
        let func = FuncAssembler::new(&mut asm, params).assemble(&body).map_err(|e| format!("fn {name}: {e}"))?;
        asm.program.funcs[index].regs = func.0;
        asm.program.funcs[index].code = func.1;
    }
    let methods = asm.program.methods.len();
    for table in asm.program.struct_methods.iter_mut().chain(asm.program.enum_methods.iter_mut()) {
        table.resize(methods, None);
    }
    Ok(asm.program)
}

fn set_method(table: &mut Vec<Option<u16>>, method: u16, func: u16) {
    if table.len() <= method as usize {
        table.resize(method as usize + 1, None);
    }
    table[method as usize] = Some(func);
}

fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        match c {
            '\\' if in_str => escaped = !escaped,
            '"' if !escaped => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => escaped = false,
        }
        if c != '\\' {
            escaped = false;
        }
    }
    line
}

fn parse_fn_header(header: &str) -> Result<(String, Vec<String>), String> {
    let header = header.trim().strip_suffix('{').ok_or("expected `{` after a function header")?.trim();
    let (name, rest) = header.split_once('(').ok_or("expected `(`")?;
    let params = rest.strip_suffix(')').ok_or("expected `)`")?;
    let params = params.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect();
    Ok((name.trim().to_string(), params))
}

/// An unresolved type: named types resolve after every declaration is read.
#[derive(Clone, Debug)]
enum TyName {
    Int,
    Float,
    Bool,
    Str,
    List(Box<TyName>),
    Map(Box<TyName>),
    Option(Box<TyName>),
    Named(String),
}

#[derive(Default)]
struct Assembler {
    program: Program,
    pending_fields: Vec<Vec<TyName>>,
    pending_payloads: Vec<Vec<Vec<TyName>>>,
    type_ids: HashMap<String, u16>,
    native_ids: HashMap<String, u16>,
}

impl Assembler {
    fn declare_type(&mut self, text: &str) -> Result<(), String> {
        let (head, body) = text.split_once('{').ok_or("expected `{`")?;
        let body = body.trim().strip_suffix('}').ok_or("expected `}`")?;
        let mut head = head.split_whitespace();
        let kind = head.next().unwrap_or_default();
        let name = head.next().ok_or("expected a type name")?.to_string();
        let items = split_top_level(body);
        // Decoders track the fields they have seen in a 64-bit mask.
        if items.iter().map(|i| split_top_level(i.split_once('(').map_or("", |(_, p)| p)).len()).chain([items.len()]).any(|n| n > 64) {
            return Err(format!("`{name}` has more than 64 fields"));
        }
        if kind == "struct" {
            let mut fields = Vec::new();
            let mut types = Vec::new();
            for item in items {
                let (var, item) = match item.strip_prefix("var ") {
                    Some(rest) => (true, rest.trim()),
                    None => (false, item.as_str()),
                };
                let (field, ty) = item.split_once(':').ok_or_else(|| format!("expected `name: type` in `{item}`"))?;
                fields.push(FieldDef { name: field.trim().to_string(), ty: Ty::Int, var });
                types.push(parse_ty(ty.trim())?);
            }
            let mutable = fields.iter().any(|f| f.var);
            self.program.structs.push(StructDef { name, fields, mutable });
            self.pending_fields.push(types);
        } else {
            let enum_id = self.program.enums.len() as u16;
            let mut variants = Vec::new();
            let mut payload_types = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let tag = self.program.tags.len() as u16;
                self.program.tags.push(TagInfo { enum_id, index: index as u16 });
                let (variant, payload) = match item.split_once('(') {
                    Some((v, p)) => (v.trim(), split_top_level(p.trim().strip_suffix(')').ok_or("expected `)`")?)),
                    None => (item.trim(), Vec::new()),
                };
                let named = payload.iter().all(|p| p.contains(':')) && !payload.is_empty();
                let mut names = Vec::new();
                let mut types = Vec::new();
                for p in &payload {
                    let ty = match p.split_once(':') {
                        Some((n, t)) if named => {
                            names.push(n.trim().to_string());
                            t
                        }
                        _ => p.as_str(),
                    };
                    types.push(parse_ty(ty.trim())?);
                }
                variants.push(VariantDef { name: variant.to_string(), field_names: named.then_some(names), payload: Vec::new(), tag });
                payload_types.push(types);
            }
            self.program.enums.push(EnumDef { name, variants });
            self.pending_payloads.push(payload_types);
        }
        Ok(())
    }

    fn resolve_types(&mut self) -> Result<(), String> {
        for (s, types) in std::mem::take(&mut self.pending_fields).into_iter().enumerate() {
            for (f, ty) in types.into_iter().enumerate() {
                self.program.structs[s].fields[f].ty = self.resolve(&ty)?;
            }
        }
        for (e, variants) in std::mem::take(&mut self.pending_payloads).into_iter().enumerate() {
            for (v, types) in variants.into_iter().enumerate() {
                let payload = types.iter().map(|t| self.resolve(t)).collect::<Result<_, _>>()?;
                self.program.enums[e].variants[v].payload = payload;
            }
        }
        Ok(())
    }

    fn resolve(&self, ty: &TyName) -> Result<Ty, String> {
        Ok(match ty {
            TyName::Int => Ty::Int,
            TyName::Float => Ty::Float,
            TyName::Bool => Ty::Bool,
            TyName::Str => Ty::Str,
            TyName::List(t) => Ty::List(Box::new(self.resolve(t)?)),
            TyName::Map(t) => Ty::Map(Box::new(self.resolve(t)?)),
            TyName::Option(t) => Ty::Option(Box::new(self.resolve(t)?)),
            TyName::Named(name) => match (self.program.struct_id(name), self.program.enum_id(name)) {
                (Some(id), _) => Ty::Struct(id),
                (_, Some(id)) => Ty::Enum(id),
                _ => return Err(format!("unknown type `{name}`")),
            },
        })
    }

    fn method(&mut self, name: &str) -> u16 {
        match self.program.methods.iter().position(|m| m == name) {
            Some(id) => id as u16,
            None => {
                self.program.methods.push(name.to_string());
                (self.program.methods.len() - 1) as u16
            }
        }
    }

    fn native(&mut self, name: &str) -> u16 {
        let next = self.native_ids.len() as u16;
        *self.native_ids.entry(name.to_string()).or_insert_with(|| {
            self.program.natives.push(name.to_string());
            next
        })
    }

    fn type_operand(&mut self, text: &str) -> Result<u16, String> {
        if let Some(&id) = self.type_ids.get(text) {
            return Ok(id);
        }
        let ty = self.resolve(&parse_ty(text)?)?;
        let id = self.program.types.len() as u16;
        self.program.types.push(ty);
        self.type_ids.insert(text.to_string(), id);
        Ok(id)
    }

    fn constant(&mut self, c: Const) -> u16 {
        match self.program.consts.iter().position(|k| *k == c) {
            Some(i) => i as u16,
            None => {
                self.program.consts.push(c);
                (self.program.consts.len() - 1) as u16
            }
        }
    }
}

/// Splits `text` at commas outside brackets and parentheses.
fn split_top_level(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                items.push(current.trim().to_string());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    if !current.trim().is_empty() {
        items.push(current.trim().to_string());
    }
    items
}

fn parse_ty(text: &str) -> Result<TyName, String> {
    let text = text.trim();
    if let Some(inner) = text.strip_suffix('?') {
        return Ok(TyName::Option(Box::new(parse_ty(inner)?)));
    }
    if let Some(inner) = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')) {
        return Ok(match inner.split_once(':') {
            Some((key, value)) if key.trim() == "str" => TyName::Map(Box::new(parse_ty(value)?)),
            Some(_) => return Err(format!("map keys must be `str` in `{text}`")),
            None => TyName::List(Box::new(parse_ty(inner)?)),
        });
    }
    Ok(match text {
        "int" => TyName::Int,
        "float" => TyName::Float,
        "bool" => TyName::Bool,
        "str" => TyName::Str,
        "" => return Err("empty type".into()),
        name => TyName::Named(name.to_string()),
    })
}

/// Splits an instruction line into tokens. String literals keep their quotes.
fn tokenize(line: &str) -> Result<Vec<String>, String> {
    let mut tokens = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' {
            chars.next();
            let mut s = String::from("\"");
            loop {
                match chars.next().ok_or("unclosed string")? {
                    '"' => break,
                    '\\' => match chars.next().ok_or("unclosed string")? {
                        'n' => s.push('\n'),
                        't' => s.push('\t'),
                        other => s.push(other),
                    },
                    other => s.push(other),
                }
            }
            tokens.push(s);
        } else {
            let mut t = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                t.push(c);
                chars.next();
            }
            tokens.push(t);
        }
    }
    Ok(tokens)
}

/// Operand shapes of an instruction.
#[derive(Clone, Copy, PartialEq)]
enum Shape {
    /// Destination register followed by `n` fixed register operands.
    Regs(usize),
    /// Destination register and a literal or symbol operand.
    DstSymbol,
    /// Destination register, a symbol, then argument registers.
    DstSymbolArgs,
    /// Destination register then argument registers.
    DstArgs,
    /// Destination register, a callee register, then argument registers.
    DstCalleeArgs,
    /// Register operands without a destination.
    Uses(usize),
    /// A symbol followed by one register.
    SymbolUse,
    /// A register, a symbol, then a register (`setfield`).
    SetField,
    /// Destination register, source register, then a symbol (`field`, `vfield`).
    DstRegSymbol,
    /// A label.
    Label,
    /// A register then a label.
    CondLabel,
    /// Destination then the rest of the line as a type.
    DstType,
}

fn shape(mnemonic: &str) -> Option<Shape> {
    use Shape::*;
    Some(match mnemonic {
        "unit" | "none" | "true" | "false" | "map" => Regs(0),
        "mov" | "sqrt" | "tofloat" | "not" | "itos" | "isnone" | "vindex" | "len" | "cell" | "cellget" | "listcap" | "pop" => Regs(1),
        "add" | "sub" | "mul" | "div" | "rem" | "fadd" | "fsub" | "fmul" | "fdiv" | "shl" | "shr" | "band" | "bxor" | "lt" | "le"
        | "eq" | "concat" | "repeat" | "index" | "mapget" | "mapkey" | "mapval" => Regs(2),
        "int" | "float" | "str" | "tag" | "cap" | "gget" => DstSymbol,
        "call" | "callm" | "native" | "closure" | "new" | "variant" => DstSymbolArgs,
        "list" => DstArgs,
        "callc" => DstCalleeArgs,
        "ret" => Uses(1),
        "cellset" | "push" => Uses(2),
        "setindex" | "mapset" => Uses(3),
        "gset" => SymbolUse,
        "setfield" => SetField,
        "field" | "vfield" => DstRegSymbol,
        "jmp" => Label,
        "jt" | "jf" => CondLabel,
        "ty" => DstType,
        _ => return None,
    })
}

struct FuncAssembler<'a> {
    asm: &'a mut Assembler,
    regs: HashMap<String, Reg>,
    next_reg: Reg,
    labels: HashMap<String, u32>,
    code: Vec<Op>,
    fixups: Vec<(usize, String)>,
    max_args: usize,
}

impl<'a> FuncAssembler<'a> {
    fn new(asm: &'a mut Assembler, params: Vec<String>) -> Self {
        let mut regs = HashMap::new();
        for (i, p) in params.iter().enumerate() {
            regs.insert(p.clone(), i as Reg);
        }
        let next_reg = params.len() as Reg;
        FuncAssembler { asm, regs, next_reg, labels: HashMap::new(), code: Vec::new(), fixups: Vec::new(), max_args: 0 }
    }

    fn reg(&mut self, name: &str) -> Result<Reg, String> {
        if name.starts_with('"') || name.starts_with('@') || name.parse::<f64>().is_ok() {
            return Err(format!("expected a register name, found `{name}`"));
        }
        if let Some(&r) = self.regs.get(name) {
            return Ok(r);
        }
        let r = self.next_reg;
        self.next_reg = r.checked_add(1).ok_or("a function has more than 65,535 registers")?;
        self.regs.insert(name.to_string(), r);
        Ok(r)
    }

    fn assemble(mut self, body: &[(usize, String)]) -> Result<(u16, Vec<Op>), String> {
        // First pass: give every register name its slot so that the argument
        // area can start above all of them.
        let mut parsed = Vec::new();
        for (line_no, line) in body {
            if line.is_empty() {
                continue;
            }
            let at = |e: String| format!("line {line_no}: {e}");
            if let Some(label) = line.strip_prefix('@') {
                parsed.push((*line_no, vec![format!("@{label}")]));
                continue;
            }
            let tokens = tokenize(line).map_err(at)?;
            let shape = shape(&tokens[0]).ok_or_else(|| at(format!("unknown instruction `{}`", tokens[0])))?;
            for (i, t) in tokens.iter().enumerate().skip(1) {
                if is_register_position(shape, i) {
                    self.reg(t).map_err(at)?;
                }
            }
            parsed.push((*line_no, tokens));
        }
        let arg_base = self.next_reg;
        for (line_no, tokens) in &parsed {
            let at = |e: String| format!("line {line_no}: {e}");
            if tokens.len() == 1 && tokens[0].starts_with('@') {
                if self.labels.insert(tokens[0][1..].to_string(), self.code.len() as u32).is_some() {
                    return Err(format!("line {line_no}: duplicate label `{}`", tokens[0]));
                }
                continue;
            }
            self.instruction(tokens, arg_base).map_err(at)?;
        }
        for (index, label) in std::mem::take(&mut self.fixups) {
            let target = *self.labels.get(&label).ok_or_else(|| format!("unknown label `@{label}`"))?;
            match &mut self.code[index] {
                Op::Jump { target: t } | Op::JumpIf { target: t, .. } | Op::JumpIfNot { target: t, .. } => *t = target,
                _ => unreachable!(),
            }
        }
        // The interpreter never runs past the last instruction or jumps outside the code.
        if !matches!(self.code.last(), Some(Op::Return { .. } | Op::Jump { .. })) {
            return Err("a function must end with `ret` or `jmp`".into());
        }
        if self.labels.values().any(|&target| target as usize >= self.code.len()) {
            return Err("a label must precede an instruction".into());
        }
        let regs = u16::try_from(arg_base as usize + self.max_args).map_err(|_| "a function has more than 65,535 registers")?;
        Ok((regs.max(1), self.code))
    }

    /// Copies `args` into the argument area and returns its base and count.
    fn args(&mut self, args: &[String], arg_base: Reg) -> Result<(Reg, u8), String> {
        let count = u8::try_from(args.len()).map_err(|_| "an instruction takes at most 255 operands".to_string())?;
        for (i, a) in args.iter().enumerate() {
            let src = self.reg(a)?;
            let dst = arg_base + i as Reg;
            self.code.push(Op::Move { dst, src });
        }
        self.max_args = self.max_args.max(args.len());
        Ok((arg_base, count))
    }

    fn instruction(&mut self, t: &[String], arg_base: Reg) -> Result<(), String> {
        let m = t[0].as_str();
        let operand = |i: usize| t.get(i).map(String::as_str).ok_or_else(|| format!("`{m}` expects more operands"));
        let r = |this: &mut Self, i: usize| -> Result<Reg, String> { this.reg(operand(i)?) };
        let op = match m {
            "unit" => Op::Unit { dst: r(self, 1)? },
            "none" => Op::None { dst: r(self, 1)? },
            "true" => Op::Bool { dst: r(self, 1)?, value: true },
            "false" => Op::Bool { dst: r(self, 1)?, value: false },
            "map" => Op::Map { dst: r(self, 1)? },
            "mov" => Op::Move { dst: r(self, 1)?, src: r(self, 2)? },
            "tofloat" => Op::ToFloat { dst: r(self, 1)?, src: r(self, 2)? },
            "not" => Op::Not { dst: r(self, 1)?, src: r(self, 2)? },
            "itos" => Op::IntToStr { dst: r(self, 1)?, src: r(self, 2)? },
            "isnone" => Op::IsNone { dst: r(self, 1)?, src: r(self, 2)? },
            "vindex" => Op::VariantIndex { dst: r(self, 1)?, src: r(self, 2)? },
            "len" => Op::Len { dst: r(self, 1)?, src: r(self, 2)? },
            "cell" => Op::NewCell { dst: r(self, 1)?, src: r(self, 2)? },
            "cellget" => Op::CellGet { dst: r(self, 1)?, cell: r(self, 2)? },
            "listcap" => Op::ListWithCapacity { dst: r(self, 1)?, cap: r(self, 2)? },
            "pop" => Op::Pop { dst: r(self, 1)?, list: r(self, 2)? },
            "add" => Op::Add { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "sub" => Op::Sub { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "mul" => Op::Mul { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "div" => Op::Div { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "rem" => Op::Rem { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "fadd" => Op::FAdd { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "fsub" => Op::FSub { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "fmul" => Op::FMul { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "fdiv" => Op::FDiv { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "sqrt" => Op::Sqrt { dst: r(self, 1)?, src: r(self, 2)? },
            "shl" => Op::Shl { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "shr" => Op::Shr { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "band" => Op::BitAnd { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "bxor" => Op::BitXor { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "lt" => Op::Lt { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "le" => Op::Le { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "eq" => Op::Eq { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "concat" => Op::Concat { dst: r(self, 1)?, a: r(self, 2)?, b: r(self, 3)? },
            "repeat" => Op::Repeat { dst: r(self, 1)?, s: r(self, 2)?, n: r(self, 3)? },
            "index" => Op::Index { dst: r(self, 1)?, list: r(self, 2)?, idx: r(self, 3)? },
            "mapget" => Op::MapGet { dst: r(self, 1)?, map: r(self, 2)?, key: r(self, 3)? },
            "mapkey" => Op::MapKey { dst: r(self, 1)?, map: r(self, 2)?, idx: r(self, 3)? },
            "mapval" => Op::MapValue { dst: r(self, 1)?, map: r(self, 2)?, idx: r(self, 3)? },
            "ret" => Op::Return { src: r(self, 1)? },
            "cellset" => Op::CellSet { cell: r(self, 1)?, src: r(self, 2)? },
            "push" => Op::Push { list: r(self, 1)?, src: r(self, 2)? },
            "setindex" => Op::SetIndex { list: r(self, 1)?, idx: r(self, 2)?, src: r(self, 3)? },
            "mapset" => Op::MapSet { map: r(self, 1)?, key: r(self, 2)?, value: r(self, 3)? },
            "int" => {
                let dst = r(self, 1)?;
                let value: i64 = operand(2)?.parse().map_err(|_| format!("bad int `{}`", operand(2).unwrap_or("")))?;
                match i32::try_from(value) {
                    Ok(value) => Op::Int { dst, value },
                    Err(_) => Op::Const { dst, idx: self.asm.constant(Const::Int(value)) },
                }
            }
            "float" => {
                let dst = r(self, 1)?;
                let value: f64 = operand(2)?.parse().map_err(|_| "bad float".to_string())?;
                Op::Const { dst, idx: self.asm.constant(Const::Float(value)) }
            }
            "str" => {
                let dst = r(self, 1)?;
                let text = operand(2)?.strip_prefix('"').ok_or("expected a string literal")?.to_string();
                Op::Const { dst, idx: self.asm.constant(Const::Str(text)) }
            }
            "tag" => {
                let dst = r(self, 1)?;
                let tag = self.variant_tag(operand(2)?)?;
                if !self.asm.program.variant(tag).payload.is_empty() {
                    return Err(format!("`{}` has a payload", operand(2)?));
                }
                Op::Tag { dst, tag }
            }
            "cap" => Op::Capture { dst: r(self, 1)?, idx: operand(2)?.parse().map_err(|_| "bad capture index".to_string())? },
            "gget" => {
                let dst = r(self, 1)?;
                Op::Global { dst, idx: self.global(operand(2)?)? }
            }
            "gset" => {
                let idx = self.global(operand(1)?)?;
                Op::SetGlobal { idx, src: r(self, 2)? }
            }
            "field" | "vfield" => {
                let dst = r(self, 1)?;
                let obj = r(self, 2)?;
                if m == "vfield" {
                    Op::VariantField { dst, src: obj, idx: operand(3)?.parse().map_err(|_| "bad payload index".to_string())? }
                } else {
                    let (_, idx, _) = self.field(operand(3)?)?;
                    Op::Field { dst, obj, idx }
                }
            }
            "setfield" => {
                let obj = r(self, 1)?;
                let (_, idx, var) = self.field(operand(2)?)?;
                if !var {
                    return Err(format!("`{}` is not a `var` field", operand(2)?));
                }
                Op::SetField { obj, idx, src: r(self, 3)? }
            }
            "jmp" => {
                self.fixups.push((self.code.len(), label(operand(1)?)?));
                Op::Jump { target: 0 }
            }
            "jt" | "jf" => {
                let cond = r(self, 1)?;
                self.fixups.push((self.code.len(), label(operand(2)?)?));
                if m == "jt" { Op::JumpIf { cond, target: 0 } } else { Op::JumpIfNot { cond, target: 0 } }
            }
            "ty" => {
                let dst = r(self, 1)?;
                let text = t[2..].join(" ");
                let id = self.asm.type_operand(&text)?;
                Op::Int { dst, value: id as i32 }
            }
            "list" => {
                let dst = r(self, 1)?;
                let (base, count) = self.args(&t[2..], arg_base)?;
                Op::List { dst, base, count }
            }
            "callc" => {
                let dst = r(self, 1)?;
                let callee = r(self, 2)?;
                let (base, argc) = self.args(&t[3..], arg_base)?;
                Op::CallClosure { dst, callee, base, argc }
            }
            "call" | "callm" | "native" | "closure" | "new" | "variant" => {
                let dst = r(self, 1)?;
                let symbol = operand(2)?.to_string();
                let (base, count) = self.args(&t[3..], arg_base)?;
                match m {
                    "call" | "closure" => {
                        let func = self.asm.program.func(&symbol).ok_or_else(|| format!("unknown function `{symbol}`"))?;
                        if m == "call" { Op::Call { dst, func, base, argc: count } } else { Op::Closure { dst, func, base, count } }
                    }
                    "callm" if count == 0 => return Err("`callm` needs a receiver".into()),
                    "callm" => Op::CallMethod { dst, method: self.asm.method(&symbol), base, argc: count },
                    "native" => Op::CallNative { dst, native: self.asm.native(&symbol), base, argc: count },
                    "new" => {
                        let ty = self.asm.program.struct_id(&symbol).ok_or_else(|| format!("unknown struct `{symbol}`"))?;
                        let fields = self.asm.program.structs[ty as usize].fields.len();
                        if fields != count as usize {
                            return Err(format!("`{symbol}` has {fields} fields, found {count}"));
                        }
                        Op::Struct { dst, ty, base, count }
                    }
                    _ => {
                        let tag = self.variant_tag(&symbol)?;
                        let payload = self.asm.program.variant(tag).payload.len();
                        if payload != count as usize {
                            return Err(format!("`{symbol}` has {payload} payload fields, found {count}"));
                        }
                        Op::Variant { dst, tag, base, count }
                    }
                }
            }
            other => return Err(format!("unknown instruction `{other}`")),
        };
        self.code.push(op);
        Ok(())
    }

    fn global(&self, name: &str) -> Result<u16, String> {
        self.asm.program.global(name).ok_or_else(|| format!("unknown global `{name}`"))
    }

    fn variant_tag(&self, path: &str) -> Result<u16, String> {
        let (ty, name) = path.split_once('.').ok_or_else(|| format!("expected `Enum.Variant`, found `{path}`"))?;
        let id = self.asm.program.enum_id(ty).ok_or_else(|| format!("unknown enum `{ty}`"))?;
        let variant = self.asm.program.enums[id as usize].variants.iter().find(|v| v.name == name);
        variant.map(|v| v.tag).ok_or_else(|| format!("unknown variant `{path}`"))
    }

    /// Resolves `Type.field` to its struct, field index, and `var` flag.
    fn field(&self, path: &str) -> Result<(u16, u16, bool), String> {
        let (ty, name) = path.split_once('.').ok_or_else(|| format!("expected `Type.field`, found `{path}`"))?;
        let id = self.asm.program.struct_id(ty).ok_or_else(|| format!("unknown struct `{ty}`"))?;
        let fields = &self.asm.program.structs[id as usize].fields;
        let idx = fields.iter().position(|f| f.name == name).ok_or_else(|| format!("unknown field `{path}`"))?;
        Ok((id, idx as u16, fields[idx].var))
    }
}

fn label(token: &str) -> Result<String, String> {
    token.strip_prefix('@').map(str::to_string).ok_or_else(|| format!("expected a label, found `{token}`"))
}

/// Returns whether operand `i` of an instruction with `shape` names a register.
fn is_register_position(shape: Shape, i: usize) -> bool {
    use Shape::*;
    match shape {
        Regs(n) => i <= n + 1,
        DstSymbol | DstType => i == 1,
        DstSymbolArgs => i != 2,
        DstArgs | DstCalleeArgs => true,
        Uses(_) => true,
        SymbolUse => i == 2,
        SetField => i != 2,
        DstRegSymbol => i <= 2,
        Label => false,
        CondLabel => i == 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_code_that_runs_past_its_end() {
        assert!(assemble("fn f() {\n    int x 1\n}").unwrap_err().contains("must end with"));
        assert!(assemble("fn f() {\n    jmp @end\n@end\n}").is_err());
        assert!(assemble("fn f() {\n    int x 1\n    ret x\n}").is_ok());
        assert!(assemble("fn f() {\n@a\n@a\n    ret x\n}").unwrap_err().contains("duplicate label"));
        assert!(assemble("fn f() {\n    callm r encode\n    ret r\n}").unwrap_err().contains("receiver"));
    }

    #[test]
    fn assembles_types_methods_and_calls() {
        let src = r#"
struct Address { street: str, city: str, zip: str? }
enum Status { Active, Suspended(reason: str), Banned(int, str) }
global g
fn Address.encode(self, enc) {
    field s self Address.street
    ret s
}
fn main(n) {
    str s "a # b"
    int i 0
@loop
    lt c i n
    jf c @done
    add i i one
    jmp @loop
@done
    variant v Status.Banned i s
    call r main i
    callm r encode r i
    ty t [str: Address?]
    ret r
}
"#;
        let p = assemble(src).unwrap();
        assert_eq!(p.structs[0].fields[2].ty, Ty::Option(Box::new(Ty::Str)));
        assert_eq!(p.enums[0].variants[1].field_names.as_deref(), Some(&["reason".to_string()][..]));
        assert_eq!(p.enums[0].variants[2].payload, vec![Ty::Int, Ty::Str]);
        assert_eq!(p.struct_method(0, ENCODE_METHOD), Some(0));
        assert_eq!(p.consts[0], Const::Str("a # b".into()));
        assert_eq!(p.types[0], Ty::Map(Box::new(Ty::Option(Box::new(Ty::Struct(0))))));
        let main = &p.funcs[1];
        assert!(main.code.iter().any(|op| matches!(op, Op::JumpIfNot { target: 6, .. })), "{:?}", main.code);
    }
}
