//! Arm 1 host integrations for B6 and B8, built only on the public host API.

use super::codec::{decode_with, encode_with};
use super::heap::{HostObject, Value};
use super::vm::{AsyncResume, NativeModule, Vm, YieldNow, type_error};
use crate::shared::sexpr::{SExpr, SExprWriter};
use crate::shared::suite::{HELD_STRING, HostConfig};
use crate::shared::{Fail, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A host object that stores a Sand callback across calls.
struct CallbackHost {
    callback: Value,
    calls: i64,
    drops: Arc<AtomicUsize>,
}

impl HostObject for CallbackHost {
    fn trace(&self, visit: &mut dyn FnMut(Value)) {
        visit(self.callback);
    }
}

impl Drop for CallbackHost {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

fn int_arg(v: Value) -> Result<i64> {
    match v {
        Value::Int(i) => Ok(i),
        _ => Err(type_error("int")),
    }
}

pub fn host_module(config: &HostConfig) -> NativeModule {
    static SEXPR: SExpr = SExpr;
    let mut m = NativeModule::default();

    m.function("list.map", |vm, args| {
        let (list, f) = (vm.arg(args, 0), vm.arg(args, 1));
        let out = vm.new_list(vm.list_len(list)?)?;
        let out = vm.push_root(out)?;
        let mut i = 0;
        while let Some(item) = vm.list_get(list, i)? {
            let mapped = vm.call(f, &[item])?;
            let mark = vm.push_root(mapped)?;
            vm.list_push(vm.root(out), mapped)?;
            vm.truncate_roots(mark);
            i += 1;
        }
        Ok(vm.root(out))
    });

    m.function("sexpr.encode", |vm, args| {
        let v = vm.arg(args, 0);
        encode_with(vm, SExprWriter::default(), v)
    });
    m.function("sexpr.decode", |vm, args| {
        let (text, ty) = (vm.arg(args, 0), int_arg(vm.arg(args, 1))?);
        decode_with(vm, &SEXPR, text, ty as u16)
    });

    let drops = config.host_drops.clone();
    m.function("host.new", move |vm, _| vm.new_host(Box::new(CallbackHost { callback: Value::Unit, calls: 0, drops: drops.clone() })));
    m.function("host.set", |vm, args| {
        let (host, callback) = (vm.arg(args, 0), vm.arg(args, 1));
        vm.host_mut::<CallbackHost>(host)?.callback = callback;
        Ok(Value::Unit)
    });
    m.function("host.fire", |vm, args| {
        let (host, x) = (vm.arg(args, 0), vm.arg(args, 1));
        let host = vm.host_mut::<CallbackHost>(host)?;
        host.calls += 1;
        let callback = host.callback;
        vm.call(callback, &[x])
    });
    m.function("host.calls", |vm, args| Ok(Value::Int(vm.host::<CallbackHost>(vm.arg(args, 0))?.calls)));

    m.async_function("async.hold_user", |vm, args| {
        let i = int_arg(vm.arg(args, 0))?;
        let make_user = vm.program.func("make_user").ok_or_else(|| Fail::error("no make_user"))?;
        let user = vm.call_function(make_user, Value::Unit, &[Value::Int(i)])?;
        let user = vm.async_root(user)?;
        let held = vm.new_str(HELD_STRING)?;
        let held = vm.async_root(held)?;
        Ok(Box::pin(async move {
            YieldNow::default().await;
            let resume: AsyncResume = Box::new(move |vm: &mut Vm| {
                let (user, held) = (vm.take_async_root(user), vm.take_async_root(held));
                let (name, email) = (vm.field(user, 1)?, vm.field(user, 2)?);
                let text = format!("{}|{}|{}", vm.str(&name)?, vm.str(&email)?, vm.str(&held)?);
                vm.new_str(&text)
            });
            resume
        }))
    });
    m
}
