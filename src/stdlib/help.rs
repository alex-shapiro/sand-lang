//! The `help` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::Vm;
use crate::vm::value::{Obj, Value};

pub fn register(r: &mut Registry) {
    r.source("help", ModKind::Std, include_str!("help.sand"));
    r.add_builtin("help.doc", |vm, a| {
        let doc = fn_help(vm, a[0]).and_then(|(doc, _)| doc).unwrap_or_default().to_string();
        Ok(vm.new_str(doc))
    });
    r.add_builtin("help.signature", |vm, a| {
        let sig = fn_help(vm, a[0]).map(|(_, sig)| sig).unwrap_or_default().to_string();
        Ok(vm.new_str(sig))
    });
    r.add_builtin("help.module", |vm, a| {
        let name = vm.str(a[0]).to_string();
        let prog = vm.prog.clone();
        // (Every standard and host module is compiled into the program.)
        let summary = prog.module_help.iter().find(|(m, _)| *m == name).map(|(_, s)| s.clone());
        let v = summary.map(|s| vm.new_str(s));
        Ok(vm.option(v))
    });
    r.add_builtin("help.modules", |vm, _| {
        let prog = vm.prog.clone();
        let items = prog.available_modules.iter().map(|m| vm.new_str(m.as_str())).collect();
        Ok(vm.heap.new_list(items))
    });
}

/// The doc comment and signature of a function value.
fn fn_help(vm: &Vm, f: Value) -> Option<(Option<&str>, &str)> {
    let entry = match vm.heap.get(f) {
        Obj::Closure(p, _) => {
            let proto = vm.prog.protos.get(*p as usize)?;
            (proto.doc.as_deref(), proto.signature.as_str())
        }
        Obj::NativeFn(n) => {
            let help = vm.prog.native_help.get(*n as usize)?;
            (help.doc.as_deref(), help.signature.as_str())
        }
        _ => return None,
    };
    Some(entry)
}
