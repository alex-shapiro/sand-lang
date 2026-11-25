//! Arm 1: a flat `Vec<Obj>` heap indexed by `Copy` handles, with an
//! explicit root stack for values that native code holds.

pub mod codec;
pub mod heap;
pub mod host;
pub mod vm;

use crate::shared::isa::Program;
use crate::shared::suite::{Arg, ArmEngine, HostConfig, HostValue, MemStats};
use crate::shared::{Fail, Limits, Result};
use std::future::Future;
use std::sync::Arc;
use vm::Vm;

pub struct Engine {
    pub vm: Vm,
}

impl ArmEngine for Engine {
    const NAME: &'static str = "arm1";

    fn new(program: Arc<Program>, limits: Limits, host: &HostConfig) -> Result<Self> {
        Ok(Engine { vm: Vm::new(program, limits, vec![codec::core_module(), host::host_module(host)])? })
    }

    fn call(&mut self, name: &str, args: &[Arg]) -> Result<HostValue> {
        self.vm.call_named(name, args)
    }

    fn collect(&mut self) {
        self.vm.collect();
    }

    fn stats(&self) -> MemStats {
        MemStats {
            tracked_bytes: self.vm.heap.tracker.used(),
            live_objects: self.vm.heap.live_objects,
            collections: self.vm.heap.policy.collections,
        }
    }

    fn pause_gc(&mut self, paused: bool) {
        self.vm.pause_gc(paused);
    }

    fn spawn(&mut self, name: &str, args: &[Arg]) -> Result<()> {
        let func = self.vm.program.func(name).ok_or_else(|| Fail::error(format!("unknown function `{name}`")))?;
        let mark = self.vm.root_mark();
        for a in args {
            let v = self.vm.from_arg(a)?;
            self.vm.push_root(v)?;
        }
        let values = self.vm.roots_from(mark).to_vec();
        let result = self.vm.spawn(func, &values);
        self.vm.truncate_roots(mark);
        result
    }

    fn run_fibers(&mut self) -> impl Future<Output = Vec<Result<HostValue>>> + Send + '_ {
        self.vm.run_fibers()
    }
}
