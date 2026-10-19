//! Interrupting engines: the engine's own flag, set through an
//! [`crate::InterruptHandle`], and a host's [`InterruptSignal`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A request from the host to stop an engine. Set it with
/// [`crate::Config::interrupt_signal`]. The engine reads the signal where it
/// reads its interrupt flag: between slices of execution, during
/// compilation, and when it wakes while it waits.
///
/// Use a signal when the engine is busy on a thread that the host cannot
/// call into, for example a Web Worker that shares memory with its page. An
/// engine that waits does not read the signal until something wakes it. To
/// stop such an engine, also call [`crate::InterruptHandle::interrupt`].
pub trait InterruptSignal: Send + Sync {
    /// Returns true if the host tells the engine to stop.
    fn is_set(&self) -> bool;
    /// Clears the request. The engine calls this when a run or a session
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
