//! A disposable prototype that compares three VM and GC designs on one
//! workload. See `experiments/vm_gc_experiment.md`.

#![forbid(unsafe_code)]

pub mod arm1;
pub mod arm3;
pub mod shared;
