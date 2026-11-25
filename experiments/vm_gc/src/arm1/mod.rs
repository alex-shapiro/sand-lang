//! Arm 1: a flat `Vec<Obj>` heap indexed by `Copy` handles, with an
//! explicit root stack for values that native code holds.

pub mod codec;
pub mod codec_objects;
pub mod heap;
pub mod host;
pub mod vm;

use crate::shared::isa::Program;
use crate::shared::suite::{Arg, ArmEngine, HostConfig, HostValue, MemStats};
use crate::shared::{Limits, Result};
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
            custom_calls: self.vm.custom_calls,
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
