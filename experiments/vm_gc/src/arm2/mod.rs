//! Arm 2: the Arm 1 heap and interpreter with Nova-style `GcScope` and
//! `NoGcScope` lifetime tokens on the host API.

pub mod codec;
pub mod host;
pub mod scope;

use crate::arm1::vm::Vm;
use crate::shared::isa::Program;
use crate::shared::suite::{Arg, ArmEngine, HostConfig, HostValue, MemStats};
use crate::shared::{Limits, Result};
use std::future::Future;
use std::sync::Arc;

pub struct Engine {
    vm: Vm,
}

impl ArmEngine for Engine {
    const NAME: &'static str = "arm2";

    fn new(program: Arc<Program>, limits: Limits, host: &HostConfig) -> Result<Self> {
        let modules = vec![codec::core_module().into_raw(), host::host_module(host).into_raw()];
        Ok(Engine { vm: Vm::new(program, limits, modules)? })
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
        self.vm.spawn_named(name, args)
    }

    fn run_fibers(&mut self) -> impl Future<Output = Vec<Result<HostValue>>> + Send + '_ {
        self.vm.run_fibers()
    }
}
