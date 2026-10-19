//! Interrupting engines: the engine's own flag, set through an
//! [`crate::InterruptHandle`], and a host's [`InterruptSignal`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A host's own request to stop an engine, which the engine polls wherever
/// it checks its interrupt flag: between slices of execution, during
/// compilation, and when it wakes while waiting. Set it with
/// [`crate::Config::interrupt_signal`].
///
/// Polling reaches an engine that is busy on a thread the host cannot call
/// into, such as a Web Worker whose page shares memory with it. A waiting
/// engine does not poll until something wakes it: to stop one, also call
/// [`crate::InterruptHandle::interrupt`].
pub trait InterruptSignal: Send + Sync {
    /// Whether the host asks the engine to stop.
    fn is_set(&self) -> bool;
    /// Forgets the request. The engine calls this when a run or session
    /// input starts.
    fn clear(&self);
}

/// A flag the host sets from any thread.
impl InterruptSignal for AtomicBool {
    fn is_set(&self) -> bool {
        self.load(Ordering::Relaxed)
    }

    fn clear(&self) {
        self.store(false, Ordering::SeqCst);
    }
}

/// An engine's interrupt state: its own flag and the host's signal.
#[derive(Default)]
pub(crate) struct Interrupt {
    flag: AtomicBool,
    signal: Option<Arc<dyn InterruptSignal>>,
}

impl Interrupt {
    pub(crate) fn new(signal: Option<Arc<dyn InterruptSignal>>) -> Arc<Interrupt> {
        Arc::new(Interrupt { flag: AtomicBool::new(false), signal })
    }

    /// Whether the engine was asked to stop.
    #[inline]
    pub(crate) fn is_set(&self) -> bool {
        self.flag.load(Ordering::Relaxed) || self.signal.as_ref().is_some_and(|s| s.is_set())
    }

    pub(crate) fn set(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }

    /// Forgets earlier requests to stop.
    pub(crate) fn clear(&self) {
        self.flag.store(false, Ordering::SeqCst);
        if let Some(s) = &self.signal {
            s.clear();
        }
    }
}
