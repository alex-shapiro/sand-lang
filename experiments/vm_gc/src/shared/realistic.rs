//! Runs the ports of `bench/programs` and returns what each program prints.

use super::programs::realistic;
use super::suite::{ArmEngine, HostConfig, HostValue, int};
use super::{Limits, Result};

/// Runs the port of `name` at `size` and returns its printed output. The
/// sizes of the original programs are `binary_trees` 15, `fannkuch` 9,
/// `nbody` 500,000, and `richards` 40.
pub fn run_port<E: ArmEngine>(name: &str, size: i64) -> Result<(String, E)> {
    let mut e = E::new(realistic(name), Limits::default(), &HostConfig::default())?;
    let out = if name == "nbody" {
        e.call("setup", &[])?;
        let before = float(e.call("energy", &[])?);
        e.call("run", &[int(size)])?;
        let after = float(e.call("energy", &[])?);
        format!("{before:.9}\n{after:.9}\n")
    } else {
        match e.call("main", &[int(size)])? {
            HostValue::Str(s) => s,
            other => panic!("{name}: expected text, got {other:?}"),
        }
    };
    Ok((out, e))
}

fn float(v: HostValue) -> f64 {
    match v {
        HostValue::Float(f) => f,
        other => panic!("expected a float, got {other:?}"),
    }
}
