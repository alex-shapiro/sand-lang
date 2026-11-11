//! The `statistics` module.

use crate::check::ModKind;
use crate::native::NativeModule;
use crate::native::ty::VmVal;
use crate::vm::value::FxHashMap;
use crate::vm::{Fail, Vm};

type Floats = VmVal<Vec<f64>>;

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("statistics", ModKind::Std, include_str!("statistics.sand"));

    m.function("mean", |vm: &mut Vm, xs: Floats| -> Result<Option<f64>, Fail> { Ok(mean(floats(vm, xs)?)) });
    m.function("mean_int", |vm: &mut Vm, xs: VmVal<Vec<i64>>| -> Result<_, Fail> {
        let items = xs.items(&vm.heap);
        vm.charge(items.len() as u64)?;
        Ok(mean(items.iter().map(|v| v.as_int() as f64)))
    });
    m.function("median", |vm: &mut Vm, xs: Floats| -> Result<_, Fail> {
        let mut xs = copied(vm, xs)?;
        let n = xs.len();
        Ok(match n {
            0 => None,
            _ if n % 2 == 1 => Some(nth(&mut xs, n / 2)),
            // The lower middle is the largest of the lower half.
            _ => {
                let hi = nth(&mut xs, n / 2);
                let lo = xs[..n / 2].iter().copied().max_by(f64::total_cmp).unwrap_or(hi);
                Some((lo + hi) / 2.0)
            }
        })
    });
    m.function("mode", |vm: &mut Vm, xs: Floats| -> Result<_, Fail> {
        let items = xs.items(&vm.heap);
        vm.charge(items.len() as u64)?;
        vm.check_memory(items.len().saturating_mul(32))?;
        // Count by bit pattern (with -0.0 folded into 0.0); remember the
        // first index of each value so ties go to the earliest.
        let mut counts: FxHashMap<u64, (usize, usize)> = FxHashMap::default();
        for (i, x) in items.iter().map(|v| v.as_float()).enumerate() {
            let key = if x == 0.0 { 0 } else { x.to_bits() };
            counts.entry(key).or_insert((0, i)).0 += 1;
        }
        let best = counts.values().max_by(|(c1, i1), (c2, i2)| c1.cmp(c2).then(i2.cmp(i1)));
        Ok(best.map(|&(_, i)| items[i].as_float()))
    });
    m.function("variance", |vm: &mut Vm, xs: Floats| -> Result<Option<f64>, Fail> { Ok(variance(floats(vm, xs)?, 1)) });
    m.function("stdev", |vm: &mut Vm, xs: Floats| -> Result<Option<f64>, Fail> { Ok(variance(floats(vm, xs)?, 1).map(f64::sqrt)) });
    m.function("pvariance", |vm: &mut Vm, xs: Floats| -> Result<Option<f64>, Fail> { Ok(variance(floats(vm, xs)?, 0)) });
    m.function("pstdev", |vm: &mut Vm, xs: Floats| -> Result<Option<f64>, Fail> { Ok(variance(floats(vm, xs)?, 0).map(f64::sqrt)) });
    m.function("min", |vm: &mut Vm, xs: Floats| -> Result<Option<f64>, Fail> { Ok(floats(vm, xs)?.reduce(f64::min)) });
    m.function("max", |vm: &mut Vm, xs: Floats| -> Result<Option<f64>, Fail> { Ok(floats(vm, xs)?.reduce(f64::max)) });
    m.function("sum", |vm: &mut Vm, xs: Floats| -> Result<f64, Fail> { Ok(sum(floats(vm, xs)?)) });
    m.function("quantile", |vm: &mut Vm, xs: Floats, q: f64| -> Result<_, Fail> {
        if xs.items(&vm.heap).is_empty() || !(0.0..=1.0).contains(&q) {
            return Ok(None);
        }
        let mut xs = copied(vm, xs)?;
        let pos = q * (xs.len() - 1) as f64;
        let lo = pos.floor() as usize;
        let frac = pos - lo as f64;
        let x = nth(&mut xs, lo);
        // The next one up is the smallest of those above `lo`.
        let y = xs[lo + 1..].iter().copied().min_by(f64::total_cmp).unwrap_or(x);
        Ok(Some(x + (y - x) * frac))
    });
    m.function("correlation", |vm: &mut Vm, xs: Floats, ys: Floats| -> Result<Option<f64>, Fail> {
        Ok(moments(floats(vm, xs)?, floats(vm, ys)?).and_then(|m| {
            let denom = (m.sxx * m.syy).sqrt();
            (denom > 0.0 && denom.is_finite()).then(|| (m.sxy / denom).clamp(-1.0, 1.0))
        }))
    });
    m.function("linear_regression", |vm: &mut Vm, xs: Floats, ys: Floats| -> Result<Option<(f64, f64)>, Fail> {
        Ok(moments(floats(vm, xs)?, floats(vm, ys)?).filter(|m| m.sxx > 0.0).map(|m| {
            let slope = m.sxy / m.sxx;
            (slope, m.mean_y - slope * m.mean_x)
        }))
    });
    m
}

/// The numbers of a list as floats, read in place, charging one unit per
/// element (the statistics are linear passes over them).
fn floats(vm: &Vm, list: Floats) -> Result<impl ExactSizeIterator<Item = f64> + Clone + '_, Fail> {
    let items = list.items(&vm.heap);
    vm.charge(items.len() as u64)?;
    Ok(items.iter().map(|v| v.as_float()))
}

/// A copy of the numbers of a list, to reorder.
fn copied(vm: &Vm, list: Floats) -> Result<Vec<f64>, Fail> {
    let xs = floats(vm, list)?;
    vm.check_memory(xs.len() * std::mem::size_of::<f64>())?;
    Ok(xs.collect())
}

/// The `i`th smallest of `xs` (in the total order of floats), moving the
/// smaller ones before it (quickselect: linear time).
fn nth(xs: &mut [f64], i: usize) -> f64 {
    *xs.select_nth_unstable_by(i, f64::total_cmp).1
}

/// Neumaier's compensated summation.
fn sum(xs: impl IntoIterator<Item = f64>) -> f64 {
    let (mut s, mut c) = (0.0f64, 0.0f64);
    for x in xs {
        let t = s + x;
        c += if s.abs() >= x.abs() { (s - t) + x } else { (x - t) + s };
        s = t;
    }
    s + c
}

fn mean(xs: impl ExactSizeIterator<Item = f64>) -> Option<f64> {
    let n = xs.len();
    (n > 0).then(|| sum(xs) / n as f64)
}

/// Sum of squared deviations divided by `n - ddof`; needs more than `ddof` values.
fn variance(xs: impl ExactSizeIterator<Item = f64> + Clone, ddof: usize) -> Option<f64> {
    let n = xs.len();
    if n <= ddof {
        return None;
    }
    let m = mean(xs.clone())?;
    Some(sum(xs.map(|x| (x - m) * (x - m))) / (n - ddof) as f64)
}

struct Moments {
    mean_x: f64,
    mean_y: f64,
    sxx: f64,
    syy: f64,
    sxy: f64,
}

/// Means and centred second moments of paired samples (at least two pairs).
fn moments(xs: impl ExactSizeIterator<Item = f64> + Clone, ys: impl ExactSizeIterator<Item = f64> + Clone) -> Option<Moments> {
    if xs.len() != ys.len() || xs.len() < 2 {
        return None;
    }
    let (mean_x, mean_y) = (mean(xs.clone())?, mean(ys.clone())?);
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for (x, y) in xs.zip(ys) {
        let (dx, dy) = (x - mean_x, y - mean_y);
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    Some(Moments { mean_x, mean_y, sxx, syy, sxy })
}
