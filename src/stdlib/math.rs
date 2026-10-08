//! The `math` module.

use crate::check::ModKind;
use crate::engine::Registry;
use crate::vm::value::Value;
use crate::vm::{NResult, panic};

pub fn register(r: &mut Registry) {
    r.source("math", ModKind::Std, include_str!("math.sand"));

    macro_rules! unary {
        ($($name:literal => $f:expr),* $(,)?) => {
            $(r.add_builtin(concat!("math.", $name), |_, a| Ok(Value::Float($f(a[0].as_float()))));)*
        };
    }
    macro_rules! binary {
        ($($name:literal => $f:expr),* $(,)?) => {
            $(r.add_builtin(concat!("math.", $name), |_, a| Ok(Value::Float($f(a[0].as_float(), a[1].as_float()))));)*
        };
    }
    unary! {
        "sqrt" => f64::sqrt,
        "cbrt" => f64::cbrt,
        "exp" => f64::exp,
        "ln" => f64::ln,
        "log2" => f64::log2,
        "log10" => f64::log10,
        "sin" => f64::sin,
        "cos" => f64::cos,
        "tan" => f64::tan,
        "asin" => f64::asin,
        "acos" => f64::acos,
        "atan" => f64::atan,
        "sinh" => f64::sinh,
        "cosh" => f64::cosh,
        "tanh" => f64::tanh,
        "floor" => f64::floor,
        "ceil" => f64::ceil,
        "round" => f64::round,
        "trunc" => f64::trunc,
        "abs" => f64::abs,
        "to_radians" => f64::to_radians,
        "to_degrees" => f64::to_degrees,
    }
    binary! {
        "log" => f64::log,
        "atan2" => f64::atan2,
        "hypot" => f64::hypot,
        "pow" => f64::powf,
        "min" => f64::min,
        "max" => f64::max,
    }

    r.add_builtin("math.gcd", |_, a| int_result(gcd(a[0].as_int(), a[1].as_int())));
    r.add_builtin("math.lcm", |_, a| {
        let (x, y) = (a[0].as_int(), a[1].as_int());
        if x == 0 || y == 0 {
            return Ok(Value::Int(0));
        }
        let l = (x.unsigned_abs() / gcd(x, y)).checked_mul(y.unsigned_abs());
        int_result(l.unwrap_or(u64::MAX))
    });
    r.add_builtin("math.isqrt", |_, a| {
        let n = a[0].as_int();
        if n < 0 {
            return panic(format!("isqrt of negative number {n}"));
        }
        Ok(Value::Int(n.isqrt()))
    });
}

fn gcd(a: i64, b: i64) -> u64 {
    let (mut a, mut b) = (a.unsigned_abs(), b.unsigned_abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Converts a non-negative result to `int`, panicking if it does not fit.
fn int_result(v: u64) -> NResult {
    match i64::try_from(v) {
        Ok(i) => Ok(Value::Int(i)),
        Err(_) => panic("integer overflow"),
    }
}
