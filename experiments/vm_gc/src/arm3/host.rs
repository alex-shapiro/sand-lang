//! Arm 3 host integrations for B6 and B8, built only on the public host API.

use super::codec::{decode_with, encode_with};
use super::value::{HostObject, Value};
use super::vm::{AsyncResume, NativeModule, Vm, YieldNow, type_error};
use crate::shared::sexpr::{SExpr, SExprWriter};
use crate::shared::suite::{HELD_STRING, HostConfig};
use crate::shared::{Fail, Result};
use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A host object that stores a Sand callback across calls.
struct CallbackHost {
    callback: Value,
    calls: i64,
    drops: Arc<AtomicUsize>,
}

impl HostObject for CallbackHost {
    fn values_mut(&mut self, f: &mut dyn FnMut(&mut Value)) {
        f(&mut self.callback);
    }
}

impl Drop for CallbackHost {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

fn int_arg(v: &Value) -> Result<i64> {
    match v {
        Value::Int(i) => Ok(*i),
        _ => Err(type_error("int")),
    }
}

/// Runs `f` on the `CallbackHost` in `v`.
fn with_host<T>(v: &Value, f: impl FnOnce(&mut CallbackHost) -> T) -> Result<T> {
    let Value::Host(h) = v else { return Err(type_error("host object")) };
    let mut guard = h.lock()?;
    let host = (&mut *guard.0 as &mut dyn Any).downcast_mut::<CallbackHost>().ok_or_else(|| type_error("CallbackHost"))?;
    Ok(f(host))
}

pub fn host_module(config: &HostConfig) -> NativeModule {
    static SEXPR: SExpr = SExpr;
    let mut m = NativeModule::default();

    m.function("list.map", |vm, args| {
        let (list, f) = (vm.arg(args, 0).clone(), vm.arg(args, 1).clone());
        let Value::List(items) = &list else { return Err(type_error("list")) };
        let len = items.lock()?.0.len();
        let out = vm.new_list_with_capacity(len)?;
        let mut i = 0;
        loop {
            // Release the lock before the callback runs, so that it can use
            // the list. A `while let` would hold the lock through its body.
            let Some(item) = items.lock()?.0.get(i).cloned() else { break };
            let mapped = vm.call(&f, &[item])?;
            vm.list_push(&out, mapped)?;
            i += 1;
        }
        Ok(out)
    });

    m.function("sexpr.encode", |vm, args| {
        let v = vm.arg(args, 0).clone();
        encode_with(vm, SExprWriter::default(), &v)
    });
    m.function("sexpr.decode", |vm, args| {
        let (text, ty) = (vm.arg(args, 0).clone(), int_arg(vm.arg(args, 1))?);
        decode_with(vm, &SEXPR, &text, ty as u16)
    });

    let drops = config.host_drops.clone();
    m.function("host.new", move |vm, _| vm.new_host(Box::new(CallbackHost { callback: Value::Unit, calls: 0, drops: drops.clone() })));
    m.function("host.set", |vm, args| {
        let callback = vm.arg(args, 1).clone();
        with_host(vm.arg(args, 0), |host| host.callback = callback)?;
        Ok(Value::Unit)
    });
    m.function("host.fire", |vm, args| {
        let x = vm.arg(args, 1).clone();
        // The lock must be released before the callback runs.
        let callback = with_host(vm.arg(args, 0), |host| {
            host.calls += 1;
            host.callback.clone()
        })?;
        vm.call(&callback, &[x])
    });
    m.function("host.calls", |vm, args| Ok(Value::Int(with_host(vm.arg(args, 0), |host| host.calls)?)));

    m.async_function("async.hold_user", |vm, args| {
        let i = int_arg(vm.arg(args, 0))?;
        let make_user = vm.program.func("make_user").ok_or_else(|| Fail::error("no make_user"))?;
        let user = vm.call_function(make_user, Value::Unit, &[Value::Int(i)])?;
        let held = vm.new_str(HELD_STRING)?;
        Ok(Box::pin(async move {
            YieldNow::default().await;
            let text = (|| {
                let Value::Struct(u) = &user else { return Err(type_error("User")) };
                let Value::List(tags) = &u.data.fields[5] else { return Err(type_error("tags")) };
                let tag = tags.lock()?.0.get(1).and_then(|t| t.str().map(str::to_string)).ok_or_else(|| Fail::error("missing tag"))?;
                let (name, email) = (u.data.fields[1].str().unwrap_or_default(), u.data.fields[2].str().unwrap_or_default());
                Ok(format!("{name}|{email}|{tag}|{}", held.str().unwrap_or_default()))
            })();
            let resume: AsyncResume = Box::new(move |vm: &mut Vm| vm.new_str(&text?));
            resume
        }))
    });
    m
}
