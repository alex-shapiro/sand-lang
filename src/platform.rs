//! What the engine needs from the platform it runs on: clocks, timers, a
//! way to let other tasks of the host's executor run, entropy and deep
//! stacks for the recursive compiler passes.
//!
//! Natively these come from the standard library and tokio. On the web
//! (`wasm32-unknown-unknown` in a JavaScript host, `cfg(web)`) the clock and
//! timers come from JavaScript (`performance.now()`, `Date.now()`,
//! `setTimeout`), entropy from `crypto.getRandomValues`, and there are no
//! threads: the compiler runs on the module's own stack, whose size is set
//! when the WebAssembly module is linked (see `wasm/build.rs`).

#[cfg(web)]
use std::pin::Pin;
#[cfg(web)]
use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(not(web))]
pub use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[cfg(web)]
pub use web_time::{Instant, SystemTime, UNIX_EPOCH};

/// The CPU time (user and system) the calling thread has used, from the
/// thread's CPU clock on Unix with the `os` feature. Elsewhere (Windows, the
/// web, builds without `os`) it is the time elapsed since the first call:
/// the CPU time limit then counts elapsed time while the engine runs.
pub(crate) fn cpu_time() -> Duration {
    #[cfg(all(feature = "os", unix))]
    {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
        Duration::new(t.tv_sec.max(0) as u64, t.tv_nsec.clamp(0, 999_999_999) as u32)
    }
    #[cfg(not(all(feature = "os", unix)))]
    {
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        START.get_or_init(Instant::now).elapsed()
    }
}

/// Waits for `d`.
#[cfg(not(web))]
pub(crate) async fn sleep(d: Duration) {
    tokio::time::sleep(d).await
}

/// Waits for `d` (with JavaScript timers, which wait at most 2^31 - 1 ms).
#[cfg(web)]
pub(crate) async fn sleep(d: Duration) {
    const MAX_MS: f64 = 2_147_483_647.0;
    let mut ms = d.as_secs_f64() * 1000.0;
    loop {
        let step = ms.min(MAX_MS);
        // Only the receiver is held across the wait, so the future is `Send`.
        let fired = web::timeout(step);
        let _ = fired.await;
        ms -= step;
        if ms <= 0.0 {
            return;
        }
    }
}

#[cfg(web)]
mod web {
    use wasm_bindgen::JsValue;
    use wasm_bindgen::closure::Closure;

    /// Starts a JavaScript timer of `ms` milliseconds; the receiver resolves
    /// when it fires. (A dropped receiver leaves the timer to fire unheard.)
    pub(super) fn timeout(ms: f64) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let callback = Closure::once_into_js(move || {
            let _ = tx.send(());
        });
        let set_timeout = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("setTimeout"))
            .ok()
            .and_then(|f| wasm_bindgen::JsCast::dyn_into::<js_sys::Function>(f).ok());
        // Without `setTimeout` (no such host) the timer never fires; the
        // engine's wall-time limit and interrupts still end the wait.
        if let Some(set_timeout) = set_timeout {
            let _ = set_timeout.call2(&JsValue::UNDEFINED, &callback, &JsValue::from_f64(ms));
        }
        rx
    }
}

/// Lets other tasks of the host's executor run.
pub(crate) async fn yield_now() {
    #[cfg(not(web))]
    tokio::task::yield_now().await;
    #[cfg(web)]
    YieldNow(false).await;
}

/// Ready on its second poll, having woken its task on the first.
#[cfg(web)]
struct YieldNow(bool);

#[cfg(web)]
impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// 64 bits of entropy from the operating system (or, on the web, from
/// `crypto.getRandomValues`). Falls back to the clock if neither is
/// available.
pub(crate) fn entropy() -> u64 {
    getrandom::u64().unwrap_or_else(|_| {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        // splitmix64 finalizer, so that close clock readings differ widely.
        let mut z = (nanos as u64) ^ ((nanos >> 64) as u64);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    })
}

/// Runs `f` on a stack of at least `size` bytes: a scoped thread natively.
/// A panic in `f` becomes `Err(message)`. On the web, `f` runs on the
/// module's stack (see the module documentation) and a panic aborts.
pub(crate) fn with_stack<R: Send>(size: usize, f: impl FnOnce() -> R + Send) -> Result<R, String> {
    #[cfg(not(web))]
    {
        std::thread::scope(|s| {
            std::thread::Builder::new()
                .stack_size(size)
                .spawn_scoped(s, f)
                .map_err(|e| e.to_string())?
                .join()
                .map_err(|p| crate::unwind::panic_text(&*p))
        })
    }
    #[cfg(web)]
    {
        let _ = size;
        Ok(f())
    }
}
