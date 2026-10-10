//! The `random` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::value::{FxHashMap, NativeObject, Obj, Value, to_usize_saturating};
use crate::vm::{NResult, Rng, Vm, panic};
use std::any::Any;

/// The opaque `random.Rng` type: an independent generator.
pub(crate) struct RngObj(pub Rng);

impl NativeObject for RngObj {
    fn type_name(&self) -> &'static str {
        "Rng"
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Allocates a `random.Rng` seeded with `seed`.
pub(crate) fn new_rng(vm: &mut Vm, seed: u64) -> Value {
    vm.heap.alloc(Obj::Native(Box::new(RngObj(Rng::new(seed)))))
}

/// A generator operation; `args` excludes the receiver.
type Op = fn(&mut Vm, &mut Rng, &[Value]) -> NResult;

/// Runs `op` with the engine's generator.
fn with_engine(vm: &mut Vm, args: &[Value], op: Op) -> NResult {
    let mut rng = vm.host.rng.clone();
    let result = op(vm, &mut rng, args);
    vm.host.rng = rng;
    result
}

/// Runs `op` with the `Rng` object in `args[0]`.
fn with_object(vm: &mut Vm, args: &[Value], op: Op) -> NResult {
    let Some(RngObj(rng)) = vm.heap.native::<RngObj>(args[0]) else { return panic("not an Rng") };
    let mut rng = rng.clone();
    let result = op(vm, &mut rng, &args[1..]);
    if let Some(obj) = vm.heap.native_mut::<RngObj>(args[0]) {
        obj.0 = rng;
    }
    result
}

pub fn register(r: &mut Registry) {
    r.source("random", ModKind::Std, include_str!("random.sand"));

    r.add_builtin("random.seed", |vm, a| {
        vm.host.rng = Rng::new(a[0].as_int() as u64);
        Ok(Value::Unit)
    });
    r.add_builtin("random.rng", |vm, a| Ok(new_rng(vm, a[0].as_int() as u64)));

    macro_rules! ops {
        ($($name:literal => $op:expr),* $(,)?) => {$(
            r.add_builtin(concat!("random.", $name), |vm, a| with_engine(vm, a, $op));
            r.add_builtin(concat!("random.Rng.", $name), |vm, a| with_object(vm, a, $op));
        )*};
    }
    ops! {
        "int" => int,
        "float" => |_, rng, _| Ok(Value::Float(rng.float())),
        "bool" => |_, rng, _| Ok(Value::Bool(rng.next_u64() >> 63 == 1)),
        "choice" => choice,
        "shuffle" => shuffle,
        "sample" => sample,
        "bytes" => bytes,
        "normal" => normal,
    }
}

fn int(_: &mut Vm, rng: &mut Rng, a: &[Value]) -> NResult {
    let (lo, hi) = (a[0].as_int(), a[1].as_int());
    if lo >= hi {
        return panic(format!("random.int: empty range {lo}..{hi}"));
    }
    let span = (hi as i128 - lo as i128) as u64;
    Ok(Value::Int((lo as i128 + rng.below(span) as i128) as i64))
}

fn choice(vm: &mut Vm, rng: &mut Rng, a: &[Value]) -> NResult {
    let xs = vm.heap.list(a[0]);
    let x = (!xs.is_empty()).then(|| xs[rng.below(xs.len() as u64) as usize]);
    Ok(vm.option(x))
}

/// Fisher-Yates.
fn shuffle(vm: &mut Vm, rng: &mut Rng, a: &[Value]) -> NResult {
    vm.charge(vm.heap.list(a[0]).len() as u64)?;
    let xs = vm.heap.list_mut(a[0]);
    for i in (1..xs.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        xs.swap(i, j);
    }
    Ok(Value::Unit)
}

/// A partial Fisher-Yates shuffle of the positions, recording only those it
/// has disturbed (the element now at each).
fn sample(vm: &mut Vm, rng: &mut Rng, a: &[Value]) -> NResult {
    let items = vm.heap.list(a[0]);
    let n = items.len();
    let k = a[1].as_int().clamp(0, n as i64) as usize;
    vm.reserve(k.saturating_mul(2 * std::mem::size_of::<Value>()))?;
    vm.charge(k as u64)?;
    let mut moved: FxHashMap<usize, usize> = FxHashMap::default();
    let mut out = Vec::with_capacity(k);
    for i in 0..k {
        let j = i + rng.below((n - i) as u64) as usize;
        let at_i = moved.get(&i).copied().unwrap_or(i);
        let at_j = moved.get(&j).copied().unwrap_or(j);
        moved.insert(j, at_i);
        out.push(items[at_j]);
    }
    Ok(vm.heap.new_list(out))
}

fn bytes(vm: &mut Vm, rng: &mut Rng, a: &[Value]) -> NResult {
    let n = a[0].as_int();
    if n < 0 {
        return panic("random.bytes: negative length");
    }
    let n = to_usize_saturating(n);
    vm.reserve(n)?;
    vm.charge_slow_bytes(n)?;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let word = rng.next_u64().to_le_bytes();
        out.extend_from_slice(&word[..(n - out.len()).min(8)]);
    }
    Ok(vm.heap.new_bytes(out))
}

/// Box-Muller transform.
fn normal(_: &mut Vm, rng: &mut Rng, a: &[Value]) -> NResult {
    let (mean, stdev) = (a[0].as_float(), a[1].as_float());
    let u1 = 1.0 - rng.float(); // in (0, 1], so ln is finite
    let u2 = rng.float();
    let z = (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos();
    Ok(Value::Float(mean + stdev * z))
}
