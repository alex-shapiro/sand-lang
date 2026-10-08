//! The `statistics` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::value::Value;
use crate::vm::{NResult, Vm};
use std::collections::HashMap;

pub fn register(r: &mut Registry) {
    r.source("statistics", ModKind::Std, include_str!("statistics.sand"));

    r.add_builtin("statistics.mean", |vm, a| {
        let xs = floats(vm, a[0]);
        float_opt(vm, mean(&xs))
    });
    r.add_builtin("statistics.mean_int", |vm, a| {
        let xs: Vec<f64> = vm.heap.list(a[0]).iter().map(|v| v.as_int() as f64).collect();
        float_opt(vm, mean(&xs))
    });
    r.add_builtin("statistics.median", |vm, a| {
        let xs = sorted(vm, a[0]);
        let n = xs.len();
        let m = match n {
            0 => None,
            _ if n % 2 == 1 => Some(xs[n / 2]),
            _ => Some((xs[n / 2 - 1] + xs[n / 2]) / 2.0),
        };
        float_opt(vm, m)
    });
    r.add_builtin("statistics.mode", |vm, a| {
        let xs = floats(vm, a[0]);
        // Count by bit pattern (with -0.0 folded into 0.0); remember the
        // first index of each value so ties go to the earliest.
        let mut counts: HashMap<u64, (usize, usize)> = HashMap::new();
        for (i, &x) in xs.iter().enumerate() {
            let key = if x == 0.0 { 0 } else { x.to_bits() };
            counts.entry(key).or_insert((0, i)).0 += 1;
        }
        let best = counts.values().max_by(|(c1, i1), (c2, i2)| c1.cmp(c2).then(i2.cmp(i1)));
        float_opt(vm, best.map(|&(_, i)| xs[i]))
    });
    r.add_builtin("statistics.variance", |vm, a| {
        let xs = floats(vm, a[0]);
        float_opt(vm, variance(&xs, 1))
    });
    r.add_builtin("statistics.stdev", |vm, a| {
        let xs = floats(vm, a[0]);
        float_opt(vm, variance(&xs, 1).map(f64::sqrt))
    });
    r.add_builtin("statistics.pvariance", |vm, a| {
        let xs = floats(vm, a[0]);
        float_opt(vm, variance(&xs, 0))
    });
    r.add_builtin("statistics.pstdev", |vm, a| {
        let xs = floats(vm, a[0]);
        float_opt(vm, variance(&xs, 0).map(f64::sqrt))
    });
    r.add_builtin("statistics.min", |vm, a| {
        let m = floats(vm, a[0]).into_iter().reduce(f64::min);
        float_opt(vm, m)
    });
    r.add_builtin("statistics.max", |vm, a| {
        let m = floats(vm, a[0]).into_iter().reduce(f64::max);
        float_opt(vm, m)
    });
    r.add_builtin("statistics.sum", |vm, a| Ok(Value::Float(sum(floats(vm, a[0])))));
    r.add_builtin("statistics.quantile", |vm, a| {
        let xs = sorted(vm, a[0]);
        let q = a[1].as_float();
        if xs.is_empty() || !(0.0..=1.0).contains(&q) {
            return Ok(vm.none());
        }
        let pos = q * (xs.len() - 1) as f64;
        let lo = pos.floor() as usize;
        let hi = (lo + 1).min(xs.len() - 1);
        let frac = pos - lo as f64;
        float_opt(vm, Some(xs[lo] + (xs[hi] - xs[lo]) * frac))
    });
    r.add_builtin("statistics.correlation", |vm, a| {
        let (xs, ys) = (floats(vm, a[0]), floats(vm, a[1]));
        let r = moments(&xs, &ys).and_then(|m| {
            let denom = (m.sxx * m.syy).sqrt();
            (denom > 0.0 && denom.is_finite()).then(|| (m.sxy / denom).clamp(-1.0, 1.0))
        });
        float_opt(vm, r)
    });
    r.add_builtin("statistics.linear_regression", |vm, a| {
        let (xs, ys) = (floats(vm, a[0]), floats(vm, a[1]));
        let fit = moments(&xs, &ys).filter(|m| m.sxx > 0.0).map(|m| {
            let slope = m.sxy / m.sxx;
            (slope, m.mean_y - slope * m.mean_x)
        });
        Ok(match fit {
            Some((slope, intercept)) => {
                let t = vm.heap.new_tuple(vec![Value::Float(slope), Value::Float(intercept)]);
                vm.some(t)
            }
            None => vm.none(),
        })
    });
}

fn floats(vm: &Vm, list: Value) -> Vec<f64> {
    vm.heap.list(list).iter().map(|v| v.as_float()).collect()
}

fn sorted(vm: &Vm, list: Value) -> Vec<f64> {
    let mut xs = floats(vm, list);
    xs.sort_by(f64::total_cmp);
    xs
}

fn float_opt(vm: &mut Vm, x: Option<f64>) -> NResult {
    Ok(vm.option(x.map(Value::Float)))
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

fn mean(xs: &[f64]) -> Option<f64> {
    (!xs.is_empty()).then(|| sum(xs.iter().copied()) / xs.len() as f64)
}

/// Sum of squared deviations divided by `n - ddof`; needs more than `ddof` values.
fn variance(xs: &[f64], ddof: usize) -> Option<f64> {
    if xs.len() <= ddof {
        return None;
    }
    let m = mean(xs)?;
    Some(sum(xs.iter().map(|x| (x - m) * (x - m))) / (xs.len() - ddof) as f64)
}

struct Moments {
    mean_x: f64,
    mean_y: f64,
    sxx: f64,
    syy: f64,
    sxy: f64,
}

/// Means and centred second moments of paired samples (at least two pairs).
fn moments(xs: &[f64], ys: &[f64]) -> Option<Moments> {
    if xs.len() != ys.len() || xs.len() < 2 {
        return None;
    }
    let (mean_x, mean_y) = (mean(xs)?, mean(ys)?);
    let (mut sxx, mut syy, mut sxy) = (0.0, 0.0, 0.0);
    for (x, y) in xs.iter().zip(ys) {
        let (dx, dy) = (x - mean_x, y - mean_y);
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    Some(Moments { mean_x, mean_y, sxx, syy, sxy })
}
