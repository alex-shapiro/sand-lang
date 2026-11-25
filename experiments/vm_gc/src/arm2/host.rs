//! Arm 2 host integrations for B6 and B8, built only on the public scoped host API.

use super::codec::{decode_with, encode_with};
use super::scope::{AsyncResume, NativeModule, Value};
use crate::arm1::heap::{HostObject, Value as HeapValue};
use crate::arm1::vm::{YieldNow, type_error};
use crate::shared::Fail;
use crate::shared::sexpr::{SExpr, SExprWriter};
use crate::shared::suite::{HELD_STRING, HostConfig};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A host object that stores a Sand callback across calls.
struct CallbackHost {
    callback: Value<'static>,
    calls: i64,
    drops: Arc<AtomicUsize>,
}

impl HostObject for CallbackHost {
    fn trace(&self, visit: &mut dyn FnMut(HeapValue)) {
        self.callback.trace(visit);
    }
}

impl Drop for CallbackHost {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

pub fn host_module(config: &HostConfig) -> NativeModule {
    static SEXPR: SExpr = SExpr;
    let mut m = NativeModule::default();

    m.function("list.map", 2, |vm, args, mut gc| {
        let list = vm.arg(args, 0, gc.nogc()).unbind().scope(vm, gc.reborrow())?;
        let f = vm.arg(args, 1, gc.nogc()).unbind().scope(vm, gc.reborrow())?;
        let n = vm.list_len(list.get(gc.nogc()), gc.nogc())?;
        let out = vm.new_list(n, gc.reborrow())?.unbind().scope(vm, gc.reborrow())?;
        let mut i = 0;
        while let Some(item) = vm.list_get(list.get(gc.nogc()), i, gc.nogc())? {
            let (callee, item) = (f.get(gc.nogc()).unbind(), item.unbind());
            let mapped = vm.call(callee, &[item], gc.reborrow())?.unbind().scope(vm, gc.reborrow())?;
            let (target, mapped) = (out.get(gc.nogc()).unbind(), mapped.get(gc.nogc()).unbind());
            vm.list_push(target, mapped, gc.reborrow())?;
            i += 1;
        }
        Ok(out.get(gc.into_nogc()))
    });

    m.function("sexpr.encode", 1, |vm, args, gc| {
        let v = vm.arg(args, 0, gc.nogc()).unbind();
        encode_with(vm, SExprWriter::default(), v, gc)
    });
    m.function("sexpr.decode", 2, |vm, args, gc| {
        let text = vm.arg(args, 0, gc.nogc()).unbind();
        let ty = vm.arg(args, 1, gc.nogc()).as_int().ok_or_else(|| type_error("int"))?;
        decode_with(vm, &SEXPR, text, ty as u16, gc)
    });

    let drops = config.host_drops.clone();
    m.function("host.new", 0, move |vm, _, gc| {
        vm.new_host(Box::new(CallbackHost { callback: Value::UNIT, calls: 0, drops: drops.clone() }), gc)
    });
    m.function("host.set", 2, |vm, args, gc| {
        let (host, callback) = (vm.arg(args, 0, gc.nogc()), vm.arg(args, 1, gc.nogc()));
        vm.host_mut::<CallbackHost>(host, gc.nogc())?.callback = callback.unbind();
        Ok(Value::UNIT)
    });
    m.function("host.fire", 2, |vm, args, gc| {
        let (host, x) = (vm.arg(args, 0, gc.nogc()), vm.arg(args, 1, gc.nogc()).unbind());
        let host = vm.host_mut::<CallbackHost>(host, gc.nogc())?;
        host.calls += 1;
        let callback = host.callback;
        vm.call(callback, &[x], gc)
    });
    m.function("host.calls", 1, |vm, args, gc| Ok(Value::int(vm.host::<CallbackHost>(vm.arg(args, 0, gc.nogc()), gc.nogc())?.calls)));

    m.async_function("async.hold_user", 1, |vm, args, mut gc| {
        let i = vm.arg(args, 0, gc.nogc()).as_int().ok_or_else(|| type_error("int"))?;
        let make_user = vm.program().func("make_user").ok_or_else(|| Fail::error("no make_user"))?;
        let user = vm.call_function(make_user, &[Value::int(i)], gc.reborrow())?.unbind();
        let user = vm.async_root(user, gc.reborrow())?;
        let held = match vm.new_str(HELD_STRING, gc.reborrow()).map(Value::unbind).and_then(|held| vm.async_root(held, gc.reborrow())) {
            Ok(held) => held,
            Err(e) => {
                vm.take_async_root(user, gc.nogc());
                return Err(e);
            }
        };
        Ok(Box::pin(async move {
            YieldNow::default().await;
            let resume: AsyncResume = Box::new(move |vm, gc| {
                let (user, held) = (vm.take_async_root(user, gc.nogc()), vm.take_async_root(held, gc.nogc()));
                let (name, email, tags) = (vm.field(user, 1, gc.nogc())?, vm.field(user, 2, gc.nogc())?, vm.field(user, 5, gc.nogc())?);
                let tag = vm.list_get(tags, 1, gc.nogc())?.ok_or_else(|| Fail::error("missing tag"))?;
                let text = format!(
                    "{}|{}|{}|{}",
                    vm.str(&name, gc.nogc())?,
                    vm.str(&email, gc.nogc())?,
                    vm.str(&tag, gc.nogc())?,
                    vm.str(&held, gc.nogc())?
                );
                vm.new_str(&text, gc)
            });
            resume
        }))
    });
    m
}
