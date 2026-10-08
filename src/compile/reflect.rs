//! Runtime type descriptions for `reflect` type parameters (e.g. `json.decode`).

use crate::check::CheckedProgram;
use crate::types::{Type, TypeDefKind, TypePrinter};
use crate::vm::value::TypeDesc;

pub fn type_desc(cp: &CheckedProgram, t: &Type) -> TypeDesc {
    let mut stack = Vec::new();
    desc(cp, t, &mut stack)
}

fn desc(cp: &CheckedProgram, t: &Type, stack: &mut Vec<String>) -> TypeDesc {
    match t {
        Type::Int => TypeDesc::Int,
        Type::Float => TypeDesc::Float,
        Type::Bool => TypeDesc::Bool,
        Type::Str => TypeDesc::Str,
        Type::Bytes => TypeDesc::Bytes,
        Type::Duration => TypeDesc::Duration,
        Type::Unit => TypeDesc::Unit,
        Type::List(e) => TypeDesc::List(Box::new(desc(cp, e, stack))),
        Type::Set(e) => TypeDesc::Set(Box::new(desc(cp, e, stack))),
        Type::Map(k, v) => TypeDesc::Map(Box::new(desc(cp, k, stack)), Box::new(desc(cp, v, stack))),
        Type::Tuple(ts) => TypeDesc::Tuple(ts.iter().map(|x| desc(cp, x, stack)).collect()),
        Type::Named(d, args) => {
            if *d == cp.wk.option {
                return TypeDesc::Option(Box::new(desc(cp, &args[0], stack)));
            }
            let def = &cp.types[*d as usize];
            if def.name == "Json" && def.module_name == "json" {
                return TypeDesc::Json;
            }
            let printer = TypePrinter { defs: &cp.types, params: &cp.params, wk: cp.wk };
            let key = printer.show(t);
            if stack.contains(&key) {
                return TypeDesc::Recur(key);
            }
            let gens = def.generics.clone();
            let sub = |ty: &Type| ty.subst_params(&|p| gens.iter().position(|&g| g == p).map(|i| args[i].clone()));
            stack.push(key.clone());
            let r = match &def.kind {
                TypeDefKind::Struct { fields } => TypeDesc::Struct {
                    def: *d,
                    name: key,
                    fields: fields.iter().map(|f| (f.name.clone(), desc(cp, &sub(&f.ty), stack), f.const_default.clone())).collect(),
                },
                TypeDefKind::Enum { variants } => TypeDesc::Enum {
                    name: key,
                    variants: variants
                        .iter()
                        .map(|v| {
                            (v.name.clone(), v.rt_id, v.fields.iter().map(|x| desc(cp, &sub(x), stack)).collect(), v.field_names.clone())
                        })
                        .collect(),
                },
                TypeDefKind::Opaque => TypeDesc::Unsupported(def.name.clone()),
            };
            stack.pop();
            r
        }
        other => {
            let printer = TypePrinter { defs: &cp.types, params: &cp.params, wk: cp.wk };
            TypeDesc::Unsupported(printer.show(other))
        }
    }
}
