//! Host native modules: Rust functions callable from Sand with automatically
//! derived type signatures.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::policy::Capability;
use crate::vm::meter::ctl_message;
use crate::vm::value::{Obj, Value};
use crate::vm::{Ctl, Finisher, HostFn, NResult, NativeImpl, Vm};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Display;
use std::future::Future;
use std::sync::Arc;

/// A Rust type with a Sand equivalent.
pub trait SandType {
    fn sand_type() -> String;
}

/// Conversion from a Sand value.
pub trait FromValue: Sized {
    fn from_value(vm: &Vm, v: Value) -> Result<Self, String>;
}

/// Conversion into a Sand value.
pub trait IntoValue {
    fn into_value(self, vm: &mut Vm) -> Value;
}

macro_rules! simple {
    ($t:ty, $name:expr) => {
        impl SandType for $t {
            fn sand_type() -> String {
                $name.into()
            }
        }
    };
}

simple!(i64, "int");
simple!(f64, "float");
simple!(bool, "bool");
simple!(String, "str");
simple!((), "()");

impl FromValue for i64 {
    fn from_value(_: &Vm, v: Value) -> Result<Self, String> {
        match v {
            Value::Int(i) => Ok(i),
            other => Err(format!("expected int, found {other:?}")),
        }
    }
}
impl IntoValue for i64 {
    fn into_value(self, _: &mut Vm) -> Value {
        Value::Int(self)
    }
}
impl FromValue for f64 {
    fn from_value(_: &Vm, v: Value) -> Result<Self, String> {
        match v {
            Value::Float(f) => Ok(f),
            other => Err(format!("expected float, found {other:?}")),
        }
    }
}
impl IntoValue for f64 {
    fn into_value(self, _: &mut Vm) -> Value {
        Value::Float(self)
    }
}
impl FromValue for bool {
    fn from_value(_: &Vm, v: Value) -> Result<Self, String> {
        match v {
            Value::Bool(b) => Ok(b),
            other => Err(format!("expected bool, found {other:?}")),
        }
    }
}
impl IntoValue for bool {
    fn into_value(self, _: &mut Vm) -> Value {
        Value::Bool(self)
    }
}
impl FromValue for String {
    fn from_value(vm: &Vm, v: Value) -> Result<Self, String> {
        match vm.heap.get(v) {
            Obj::Str(s) => {
                vm.charge_bytes(s.len()).map_err(ctl_message)?;
                Ok(s.to_string())
            }
            _ => Err("expected str".into()),
        }
    }
}
impl IntoValue for String {
    fn into_value(self, vm: &mut Vm) -> Value {
        vm.heap.new_str(self)
    }
}
impl IntoValue for &str {
    fn into_value(self, vm: &mut Vm) -> Value {
        vm.heap.new_str(self)
    }
}
impl FromValue for () {
    fn from_value(_: &Vm, _: Value) -> Result<Self, String> {
        Ok(())
    }
}
impl IntoValue for () {
    fn into_value(self, _: &mut Vm) -> Value {
        Value::Unit
    }
}

impl SandType for Vec<u8> {
    fn sand_type() -> String {
        "bytes".into()
    }
}
impl FromValue for Vec<u8> {
    fn from_value(vm: &Vm, v: Value) -> Result<Self, String> {
        match vm.heap.get(v) {
            Obj::Bytes(b) => {
                vm.charge_bytes(b.len()).map_err(ctl_message)?;
                Ok(b.to_vec())
            }
            _ => Err("expected bytes".into()),
        }
    }
}
impl IntoValue for Vec<u8> {
    fn into_value(self, vm: &mut Vm) -> Value {
        vm.heap.new_bytes(self)
    }
}

macro_rules! vec_of {
    ($($t:ty),*) => {$(
        impl SandType for Vec<$t> {
            fn sand_type() -> String { format!("[{}]", <$t as SandType>::sand_type()) }
        }
        impl FromValue for Vec<$t> {
            fn from_value(vm: &Vm, v: Value) -> Result<Self, String> {
                vm.charge(vm.heap.list(v).len() as u64).map_err(ctl_message)?;
                vm.heap.list(v).iter().map(|x| <$t as FromValue>::from_value(vm, *x)).collect()
            }
        }
        impl IntoValue for Vec<$t> {
            fn into_value(self, vm: &mut Vm) -> Value {
                let items: Vec<Value> = self.into_iter().map(|x| x.into_value(vm)).collect();
                vm.heap.new_list(items)
            }
        }
    )*};
}
vec_of!(i64, f64, bool, String, Vec<u8>);

impl<T: SandType> SandType for Option<T> {
    fn sand_type() -> String {
        format!("{}?", T::sand_type())
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(vm: &Vm, v: Value) -> Result<Self, String> {
        match vm.as_option(v) {
            Some(x) => Ok(Some(T::from_value(vm, x)?)),
            None => Ok(None),
        }
    }
}
impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self, vm: &mut Vm) -> Value {
        match self {
            Some(x) => {
                let v = x.into_value(vm);
                vm.some(v)
            }
            None => vm.none(),
        }
    }
}

impl<T: SandType, E> SandType for Result<T, E> {
    fn sand_type() -> String {
        format!("Result<{}, Error>", T::sand_type())
    }
}
impl<T: IntoValue, E: Display> IntoValue for Result<T, E> {
    fn into_value(self, vm: &mut Vm) -> Value {
        match self {
            Ok(x) => {
                let v = x.into_value(vm);
                vm.ok(v)
            }
            Err(e) => vm.err_value("error", &e.to_string()),
        }
    }
}

fn map_type<T: SandType>() -> String {
    format!("[str: {}]", T::sand_type())
}

impl<T: SandType> SandType for HashMap<String, T> {
    fn sand_type() -> String {
        map_type::<T>()
    }
}
impl<T: SandType> SandType for BTreeMap<String, T> {
    fn sand_type() -> String {
        map_type::<T>()
    }
}
fn read_map<T: FromValue>(vm: &Vm, v: Value) -> Result<Vec<(String, T)>, String> {
    match vm.heap.get(v) {
        Obj::Map(m) => {
            vm.charge(m.live as u64).map_err(ctl_message)?;
            let mut out = Vec::new();
            for (k, val) in m.entries.iter().flatten() {
                out.push((String::from_value(vm, *k)?, T::from_value(vm, *val)?));
            }
            Ok(out)
        }
        _ => Err("expected map".into()),
    }
}
fn write_map<T: IntoValue>(vm: &mut Vm, items: impl IntoIterator<Item = (String, T)>) -> Value {
    let m = vm.heap.alloc(Obj::Map(Box::default()));
    for (k, v) in items {
        let kv = vm.heap.new_str(k);
        let vv = v.into_value(vm);
        let _ = vm.map_insert(m, kv, vv);
    }
    m
}
impl<T: FromValue> FromValue for HashMap<String, T> {
    fn from_value(vm: &Vm, v: Value) -> Result<Self, String> {
        Ok(read_map(vm, v)?.into_iter().collect())
    }
}
impl<T: IntoValue> IntoValue for HashMap<String, T> {
    fn into_value(self, vm: &mut Vm) -> Value {
        let mut items: Vec<(String, T)> = self.into_iter().collect();
        items.sort_by(|a, b| a.0.cmp(&b.0));
        write_map(vm, items)
    }
}
impl<T: FromValue> FromValue for BTreeMap<String, T> {
    fn from_value(vm: &Vm, v: Value) -> Result<Self, String> {
        Ok(read_map(vm, v)?.into_iter().collect())
    }
}
impl<T: IntoValue> IntoValue for BTreeMap<String, T> {
    fn into_value(self, vm: &mut Vm) -> Value {
        write_map(vm, self)
    }
}

macro_rules! tuple_impls {
    ($($n:ident $i:tt),+) => {
        impl<$($n: SandType),+> SandType for ($($n,)+) {
            fn sand_type() -> String {
                let parts: Vec<String> = vec![$($n::sand_type()),+];
                format!("({})", parts.join(", "))
            }
        }
        impl<$($n: FromValue),+> FromValue for ($($n,)+) {
            fn from_value(vm: &Vm, v: Value) -> Result<Self, String> {
                let f = vm.heap.fields(v);
                Ok(($($n::from_value(vm, *f.get($i).ok_or("tuple too short")?)?,)+))
            }
        }
        impl<$($n: IntoValue),+> IntoValue for ($($n,)+) {
            fn into_value(self, vm: &mut Vm) -> Value {
                let items = vec![$(self.$i.into_value(vm)),+];
                vm.heap.new_tuple(items)
            }
        }
    };
}
tuple_impls!(A 0, B 1);
tuple_impls!(A 0, B 1, C 2);
tuple_impls!(A 0, B 1, C 2, D 3);

/// A Rust closure usable as a native function.
pub trait HostFunction<Marker>: Send + Sync + 'static {
    fn param_types() -> Vec<String>;
    fn ret_type() -> String;
    fn into_host(self) -> HostFn;
}

/// A Rust closure returning a future, usable as an async native function.
pub trait AsyncHostFunction<Marker>: Send + Sync + 'static {
    fn param_types() -> Vec<String>;
    fn ret_type() -> String;
    fn into_host(self) -> HostFn;
}

macro_rules! host_fn_impls {
    ($($a:ident),*) => {
        #[allow(non_snake_case, unused_variables, unused_mut)]
        impl<F, R, $($a,)*> HostFunction<($($a,)*)> for F
        where
            F: Fn($($a),*) -> R + Send + Sync + 'static,
            R: IntoValue + SandType,
            $($a: FromValue + SandType,)*
        {
            fn param_types() -> Vec<String> {
                vec![$($a::sand_type()),*]
            }
            fn ret_type() -> String {
                R::sand_type()
            }
            fn into_host(self) -> HostFn {
                Arc::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
                    let mut it = args.iter();
                    $(let $a = $a::from_value(vm, *it.next().ok_or_else(|| Ctl::Panic("missing argument".into()))?).map_err(crate::vm::meter::ctl_from_message)?;)*
                    let r = self($($a),*);
                    Ok(r.into_value(vm))
                })
            }
        }

        #[allow(non_snake_case, unused_variables, unused_mut)]
        impl<F, Fut, R, $($a,)*> AsyncHostFunction<(Fut, $($a,)*)> for F
        where
            F: Fn($($a),*) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = R> + Send + 'static,
            R: IntoValue + SandType + Send + 'static,
            $($a: FromValue + SandType,)*
        {
            fn param_types() -> Vec<String> {
                vec![$($a::sand_type()),*]
            }
            fn ret_type() -> String {
                R::sand_type()
            }
            fn into_host(self) -> HostFn {
                Arc::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
                    let mut it = args.iter();
                    $(let $a = $a::from_value(vm, *it.next().ok_or_else(|| Ctl::Panic("missing argument".into()))?).map_err(crate::vm::meter::ctl_from_message)?;)*
                    let fut = self($($a),*);
                    Err(Ctl::Async(Box::pin(async move {
                        let r = fut.await;
                        Box::new(move |vm: &mut Vm| Ok(r.into_value(vm))) as Finisher
                    })))
                })
            }
        }
    };
}
host_fn_impls!();
host_fn_impls!(A);
host_fn_impls!(A, B);
host_fn_impls!(A, B, C);
host_fn_impls!(A, B, C, D);
host_fn_impls!(A, B, C, D, E);
host_fn_impls!(A, B, C, D, E, G);

const PARAM_NAMES: [&str; 6] = ["a", "b", "c", "d", "e", "f"];

/// A host-provided module, imported by scripts with `import <name>`.
#[derive(Clone)]
pub struct NativeModule {
    name: String,
    source: String,
    decls: Vec<(String, String)>,
    functions: Vec<(String, HostFn)>,
    docs: HashMap<String, String>,
}

impl NativeModule {
    pub fn new(name: &str) -> NativeModule {
        NativeModule { name: name.to_string(), source: String::new(), decls: Vec::new(), functions: Vec::new(), docs: HashMap::new() }
    }

    fn signature(name: &str, params: &[String], ret: &str) -> String {
        let ps: Vec<String> =
            params.iter().enumerate().map(|(i, t)| format!("{}: {t}", PARAM_NAMES.get(i).copied().unwrap_or("x"))).collect();
        format!("pub native fn {name}({}) -> {ret}", ps.join(", "))
    }

    /// Registers a synchronous function; its Sand signature is derived from the Rust types.
    pub fn function<M, F: HostFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        let decl = Self::signature(name, &F::param_types(), &F::ret_type());
        self.decls.push((name.to_string(), decl));
        self.functions.push((name.to_string(), f.into_host()));
        self
    }

    /// Registers an async function: the calling task is suspended until the future completes.
    pub fn async_function<M, F: AsyncHostFunction<M>>(&mut self, name: &str, f: F) -> &mut Self {
        let decl = Self::signature(name, &F::param_types(), &F::ret_type());
        self.decls.push((name.to_string(), decl));
        self.functions.push((name.to_string(), f.into_host()));
        self
    }

    /// Registers a one-argument function guarded by a capability check. The
    /// Sand return type is `Result<R, Error>`; a denial yields a
    /// `permission_denied` error.
    pub fn guarded_function<A, R>(
        &mut self,
        name: &str,
        guard: impl Fn(&A) -> Capability + Send + Sync + 'static,
        f: impl Fn(A) -> R + Send + Sync + 'static,
    ) -> &mut Self
    where
        A: FromValue + SandType + 'static,
        R: IntoValue + SandType + 'static,
    {
        let decl = Self::signature(name, &[A::sand_type()], &format!("Result<{}, Error>", R::sand_type()));
        self.decls.push((name.to_string(), decl));
        let host: HostFn = Arc::new(move |vm: &mut Vm, args: &[Value]| -> NResult {
            let a = A::from_value(vm, args[0]).map_err(crate::vm::meter::ctl_from_message)?;
            let cap = guard(&a);
            if !vm.permit(&cap)? {
                return Ok(vm.err_value("permission_denied", &format!("permission denied: {cap}")));
            }
            let v = f(a).into_value(vm);
            Ok(vm.ok(v))
        });
        self.functions.push((name.to_string(), host));
        self
    }

    /// Registers a function with direct access to the VM. Its signature must be
    /// declared in [`NativeModule::source`] with `native fn`.
    pub fn raw_function(&mut self, name: &str, f: impl Fn(&mut Vm, &[Value]) -> NResult + Send + Sync + 'static) -> &mut Self {
        self.functions.push((name.to_string(), Arc::new(f)));
        self
    }

    /// Adds Sand declarations (types, helper functions, `native fn` signatures).
    pub fn source(&mut self, src: &str) -> &mut Self {
        self.source.push_str(src);
        self.source.push('\n');
        self
    }

    pub fn doc(&mut self, name: &str, text: &str) -> &mut Self {
        self.docs.insert(name.to_string(), text.to_string());
        self
    }

    pub(crate) fn register(&self, r: &mut Registry) {
        let mut src = self.source.clone();
        for (name, decl) in &self.decls {
            if src.contains(&format!("native fn {name}(")) {
                continue;
            }
            if let Some(d) = self.docs.get(name) {
                for line in d.lines() {
                    src.push_str(&format!("/// {line}\n"));
                }
            }
            src.push_str(decl);
            src.push('\n');
        }
        r.source(&self.name, ModKind::Host, &src);
        for (name, f) in &self.functions {
            r.add(&format!("{}.{}", self.name, name), NativeImpl::Host(f.clone()));
        }
    }
}
