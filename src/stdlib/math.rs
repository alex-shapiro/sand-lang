//! The `math` module.

use crate::check::ModKind;
use crate::native::NativeModule;
use crate::vm::{Ctl, fail};

pub(crate) fn module() -> NativeModule {
    let mut m = NativeModule::std("math", ModKind::Std, include_str!("math.sand"));

    macro_rules! functions {
        ($($name:literal => $f:expr),* $(,)?) => {
            $(m.function($name, $f);)*
        };
    }
    functions! {
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
        "to_radians" => f64::to_radians,
        "to_degrees" => f64::to_degrees,
        "log" => f64::log,
        "atan2" => f64::atan2,
        "hypot" => f64::hypot,
    }

    m.function("gcd", |a: i64, b: i64| int_result(gcd(a, b)));
    m.function("lcm", |x: i64, y: i64| {
        if x == 0 || y == 0 {
            return Ok(0);
        }
        let l = (x.unsigned_abs() / gcd(x, y)).checked_mul(y.unsigned_abs());
        int_result(l.unwrap_or(u64::MAX))
    });
    m.function("isqrt", |n: i64| {
        if n < 0 {
            return fail("invalid", format!("isqrt of negative number {n}"));
        }
        Ok(n.isqrt())
    });
    m
}

fn gcd(a: i64, b: i64) -> u64 {
    let (mut a, mut b) = (a.unsigned_abs(), b.unsigned_abs());
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// Converts a non-negative result to `int`, failing if it does not fit.
fn int_result(v: u64) -> Result<i64, Ctl> {
    match i64::try_from(v) {
        Ok(i) => Ok(i),
        Err(_) => fail("overflow", "integer overflow"),
    }
}
