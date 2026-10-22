//! The `random` module.

use crate::check::ModKind;
use crate::native::{NativeModule, NativeObject, Param, Val};
use crate::vm::value::{FxHashMap, Value, to_usize_saturating};
use crate::vm::{Ctl, Rng, Vm, fail};

/// An isolated pseudo-random number generator created with `random.rng(seed)`.
#[derive(crate::SandObject)]
#[sand(module = "random", name = "Rng")]
struct RngObj(Rng);

impl NativeObject for RngObj {}

fn with_engine<R>(vm: &mut Vm, op: impl FnOnce(&mut Vm, &mut Rng) -> Result<R, Ctl>) -> Result<R, Ctl> {
    let mut rng = vm.host.rng.clone();
    let result = op(vm, &mut rng);
    vm.host.rng = rng;
    result
}

fn with_object<R>(vm: &mut Vm, obj: Val<RngObj>, op: impl FnOnce(&mut Vm, &mut Rng) -> Result<R, Ctl>) -> Result<R, Ctl> {
    let mut rng = obj.get(&vm.heap)?.0.clone();
    let result = op(vm, &mut rng);
    obj.get_mut(&mut vm.heap)?.0 = rng;
    result
}

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("random", ModKind::Std, include_str!("random.sand"));
    m.bind::<RngObj>();
    m.vm_function("seed", |vm: &mut Vm, s: i64| -> Result<_, Ctl> {
        vm.host.rng = Rng::new(s as u64);
        Ok(())
    });
    m.vm_function("rng", |vm: &mut Vm, seed: i64| vm.new_object(RngObj(Rng::new(seed as u64))));

    macro_rules! ops {
        ($($name:literal => |$($a:ident: $t:ty),*| -> $r:ty = $op:expr;)*) => {$(
            m.vm_function($name, |vm: &mut Vm, $($a: $t),*| -> Result<$r, Ctl> { with_engine(vm, |vm, rng| $op(vm, rng, $($a),*)) });
            m.vm_function(concat!("Rng.", $name), |vm: &mut Vm, obj: Val<RngObj>, $($a: $t),*| -> Result<$r, Ctl> {
                with_object(vm, obj, |vm, rng| $op(vm, rng, $($a),*))
            });
        )*};
    }
    ops! {
        "int" => |lo: i64, hi: i64| -> i64 = int;
        "float" => | | -> f64 = |_: &mut Vm, rng: &mut Rng| Ok(rng.float());
        "bool" => | | -> bool = |_: &mut Vm, rng: &mut Rng| Ok(rng.next_u64() >> 63 == 1);
        "choice" => |xs: Val<Vec<Param<0>>>| -> Option<Param<0>> = choice;
        "shuffle" => |xs: Val<Vec<Param<0>>>| -> () = shuffle;
        "sample" => |xs: Val<Vec<Param<0>>>, k: i64| -> Vec<Param<0>> = sample;
        "bytes" => |n: i64| -> Vec<u8> = bytes;
    }
    m
}

fn int(_: &mut Vm, rng: &mut Rng, lo: i64, hi: i64) -> Result<i64, Ctl> {
    if lo >= hi {
        return fail("invalid", format!("random.int: empty range {lo}..{hi}"));
    }
    let span = (hi as i128 - lo as i128) as u64;
    Ok((lo as i128 + rng.below(span) as i128) as i64)
}

fn choice(vm: &mut Vm, rng: &mut Rng, xs: Val<Vec<Param<0>>>) -> Result<Option<Param<0>>, Ctl> {
    let xs = xs.items(&vm.heap);
    Ok((!xs.is_empty()).then(|| Param::from_raw(xs[rng.below(xs.len() as u64) as usize])))
}

/// Fisher-Yates.
fn shuffle(vm: &mut Vm, rng: &mut Rng, xs: Val<Vec<Param<0>>>) -> Result<(), Ctl> {
    vm.charge(xs.items(&vm.heap).len() as u64)?;
    let xs = vm.heap.list_mut(xs.raw());
    for i in (1..xs.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        xs.swap(i, j);
    }
    Ok(())
}

/// A partial Fisher-Yates shuffle of the positions, recording only those it
/// has disturbed (the element now at each).
fn sample(vm: &mut Vm, rng: &mut Rng, xs: Val<Vec<Param<0>>>, k: i64) -> Result<Vec<Param<0>>, Ctl> {
    let items = xs.items(&vm.heap);
    let n = items.len();
    let k = k.clamp(0, n as i64) as usize;
    vm.check_room(k.saturating_mul(2 * std::mem::size_of::<Value>()))?;
    let mut moved: FxHashMap<usize, usize> = FxHashMap::default();
    let mut out = Vec::with_capacity(k);
    for i in 0..k {
        let j = i + rng.below((n - i) as u64) as usize;
        let at_i = moved.get(&i).copied().unwrap_or(i);
        let at_j = moved.get(&j).copied().unwrap_or(j);
        moved.insert(j, at_i);
        out.push(Param::from_raw(items[at_j]));
    }
    Ok(out)
}

fn bytes(vm: &mut Vm, rng: &mut Rng, n: i64) -> Result<Vec<u8>, Ctl> {
    if n < 0 {
        return fail("invalid", "random.bytes: negative length");
    }
    let n = to_usize_saturating(n);
    vm.check_room(n)?;
    vm.charge_slow_bytes(n)?;
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let word = rng.next_u64().to_le_bytes();
        out.extend_from_slice(&word[..(n - out.len()).min(8)]);
    }
    Ok(out)
}
