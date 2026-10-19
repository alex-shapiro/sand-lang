//! Platform abstractions for clocks, timers, entropy, and execution stacks.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

#[cfg(not(web))]
pub use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[cfg(web)]
pub use web_time::{Instant, SystemTime, UNIX_EPOCH};

pub type ClockFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Clocks and timers for engine execution.
pub trait Clock: Send + Sync {
    /// Returns the current system time.
    fn wall(&self) -> SystemTime;

    /// Returns the current monotonic time.
    fn now(&self) -> Instant;

    /// Returns the CPU time of the current thread.
    fn cpu_time(&self) -> Duration;

    /// Pauses execution for duration.
    fn sleep(&self, duration: Duration) -> ClockFuture;

    /// Yields execution to the host scheduler.
    fn yield_now(&self) -> ClockFuture;

    /// Records fuel units of work completed by the engine.
    fn work(&self, fuel: u64) {
        let _ = fuel;
    }
}

/// Execution deadline evaluated against a clock.
#[derive(Clone)]
pub struct Deadline {
    pub at: Instant,
    pub clock: std::sync::Arc<dyn Clock>,
}

impl Deadline {
    /// Returns true if the deadline time has passed.
    pub fn passed(&self) -> bool {
        self.clock.now() >= self.at
    }
}

/// Awaits future until duration elapses on clock.
pub(crate) async fn timeout<F: Future>(clock: &dyn Clock, duration: Duration, future: F) -> Option<F::Output> {
    let mut future = std::pin::pin!(future);
    let mut expiry = clock.sleep(duration);
    std::future::poll_fn(|cx: &mut Context<'_>| {
        if let Poll::Ready(output) = future.as_mut().poll(cx) {
            return Poll::Ready(Some(output));
        }
        expiry.as_mut().poll(cx).map(|()| None)
    })
    .await
}

/// Clock implementation using standard system clocks and timers.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn cpu_time(&self) -> Duration {
        cpu_time()
    }

    fn sleep(&self, duration: Duration) -> ClockFuture {
        Box::pin(sleep(duration))
    }

    fn yield_now(&self) -> ClockFuture {
        Box::pin(yield_now())
    }
}

/// Returns thread CPU time on Unix, or elapsed process time on other platforms.
fn cpu_time() -> Duration {
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

#[cfg(not(web))]
async fn sleep(d: Duration) {
    tokio::time::sleep(d).await
}

/// Waits for `d` (with JavaScript timers, which wait at most 2^31 - 1 ms).
#[cfg(web)]
async fn sleep(d: Duration) {
    const MAX_MS: f64 = 2_147_483_647.0;
    /// Pauses execution on WebAssembly hosts.
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

    /// Starts a JavaScript timer for ms milliseconds.
    pub(super) fn timeout(ms: f64) -> tokio::sync::oneshot::Receiver<()> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let callback = Closure::once_into_js(move || {
            let _ = tx.send(());
        });
        let set_timeout = js_sys::Reflect::get(&js_sys::global(), &JsValue::from_str("setTimeout"))
            .ok()
            .and_then(|f| wasm_bindgen::JsCast::dyn_into::<js_sys::Function>(f).ok());
        if let Some(set_timeout) = set_timeout {
            let _ = set_timeout.call2(&JsValue::UNDEFINED, &callback, &JsValue::from_f64(ms));
        }
        rx
    }
}

async fn yield_now() {
    #[cfg(not(web))]
    tokio::task::yield_now().await;
    #[cfg(web)]
    YieldNow(false).await;
}

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

/// The stack size of the threads [`on_deep_stack`] runs work on.
#[cfg(not(web))]
const DEEP_STACK: usize = 256 << 20;

#[cfg(not(web))]
static DEEP: pool::Pool = pool::Pool::new("sand-compile", Some(DEEP_STACK));

#[cfg(not(web))]
std::thread_local! {
    /// True if the current thread is in the DEEP pool.
    static ON_DEEP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `f` on a large stack for recursive compiler operations.
pub(crate) fn on_deep_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> std::thread::Result<R> {
    #[cfg(not(web))]
    {
        if ON_DEEP.get() {
            return std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        }
        let job = move || {
            ON_DEEP.set(true);
            f()
        };
        match DEEP.run(job) {
            Ok(pending) => pending.wait_raw(),
            Err((_, e)) => Err(Box::new(format!("cannot start a compiler thread: {e}"))),
        }
    }
    #[cfg(web)]
    {
        Ok(f())
    }
}

/// Executes a blocking operation on a background worker thread.
#[cfg(feature = "os")]
pub(crate) fn offload<T: Send + 'static>(job: impl FnOnce() -> T + Send + 'static) -> pool::Pending<T> {
    static IO: pool::Pool = pool::Pool::new("sand-io", None);
    // Run inline if thread creation fails.
    IO.run(job).unwrap_or_else(|(job, _)| pool::Pending::now(job))
}

/// Worker thread pool for background tasks.
#[cfg(not(web))]
mod pool {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::time::{Duration, Instant};

    /// How often a waiting thread checks whether to stop waiting.
    #[cfg(feature = "os")]
    const POLL: Duration = Duration::from_millis(5);
    /// Idle workers exit after this long.
    const IDLE: Duration = Duration::from_secs(20);
    /// How long a worker looks for its next job, and a waiting thread for
    /// its result, before going to sleep. Most jobs take a few
    /// microseconds, less than it takes to put a thread to sleep and wake it.
    const SPIN: Duration = Duration::from_micros(50);

    /// Spins until `ready` returns true or [`SPIN`] has passed. Returns the
    /// last result of `ready`.
    fn spin(ready: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        loop {
            for _ in 0..64 {
                if ready() {
                    return true;
                }
                std::hint::spin_loop();
            }
            if start.elapsed() >= SPIN {
                return ready();
            }
        }
    }

    type Job = Box<dyn FnOnce() + Send>;

    pub(crate) struct Pool {
        name: &'static str,
        /// The workers' stack size (`None`: the standard library's default).
        stack: Option<usize>,
        state: Mutex<Queue>,
        work: Condvar,
        /// The length of `state.jobs`, which spinning workers read without the lock.
        queued: AtomicUsize,
        /// Count of active spinning workers.
        spinning: AtomicUsize,
    }

    struct Queue {
        jobs: VecDeque<Job>,
        idle: usize,
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    impl Pool {
        pub(crate) const fn new(name: &'static str, stack: Option<usize>) -> Pool {
            Pool {
                name,
                stack,
                state: Mutex::new(Queue { jobs: VecDeque::new(), idle: 0 }),
                work: Condvar::new(),
                queued: AtomicUsize::new(0),
                spinning: AtomicUsize::new(0),
            }
        }

        /// Runs `job` on an idle worker, or spawns a worker thread.
        pub(crate) fn run<T, F>(&'static self, job: F) -> Result<Pending<T>, (F, std::io::Error)>
        where
            T: Send + 'static,
            F: FnOnce() -> T + Send + 'static,
        {
            let mut q = lock(&self.state);
            let workers = q.idle + self.spinning.load(Ordering::SeqCst);
            if q.jobs.len() >= workers {
                drop(q);
                let mut b = std::thread::Builder::new().name(self.name.into());
                if let Some(size) = self.stack {
                    b = b.stack_size(size);
                }
                if let Err(e) = b.spawn(move || self.work_loop()) {
                    return Err((job, e));
                }
                q = lock(&self.state);
            }
            let slot = Arc::new(Slot { state: Mutex::new((None, None)), done: Condvar::new(), ready: AtomicBool::new(false) });
            let theirs = slot.clone();
            q.jobs.push_back(Box::new(move || {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                let mut s = lock(&theirs.state);
                s.0 = Some(r);
                theirs.ready.store(true, Ordering::Release);
                let waker = s.1.take();
                drop(s);
                theirs.done.notify_one();
                if let Some(w) = waker {
                    w.wake();
                }
            }));
            self.queued.store(q.jobs.len(), Ordering::SeqCst);
            if q.jobs.len() > self.spinning.load(Ordering::SeqCst) {
                self.work.notify_one();
            }
            Ok(Pending(slot))
        }

        fn work_loop(&self) {
            let mut q = lock(&self.state);
            loop {
                if let Some(job) = q.jobs.pop_front() {
                    self.queued.store(q.jobs.len(), Ordering::SeqCst);
                    drop(q);
                    job();
                    self.spinning.fetch_add(1, Ordering::SeqCst);
                    spin(|| self.queued.load(Ordering::SeqCst) > 0);
                    q = lock(&self.state);
                    self.spinning.fetch_sub(1, Ordering::SeqCst);
                    continue;
                }
                q.idle += 1;
                let (guard, waited) = self.work.wait_timeout(q, IDLE).unwrap_or_else(|e| e.into_inner());
                q = guard;
                q.idle -= 1;
                if waited.timed_out() && q.jobs.is_empty() {
                    return;
                }
            }
        }
    }

    struct Slot<T> {
        state: Mutex<(Option<std::thread::Result<T>>, Option<std::task::Waker>)>,
        done: Condvar,
        ready: AtomicBool,
    }

    /// The result of a job running on a worker thread.
    pub(crate) struct Pending<T>(Arc<Slot<T>>);

    impl<T> Pending<T> {
        /// Runs `job` on this thread.
        #[cfg(feature = "os")]
        pub(crate) fn now(job: impl FnOnce() -> T) -> Pending<T> {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
            Pending(Arc::new(Slot { state: Mutex::new((Some(r), None)), done: Condvar::new(), ready: AtomicBool::new(true) }))
        }

        /// Waits for the worker result while periodically calling check.
        #[cfg(feature = "os")]
        pub(crate) fn wait<E>(self, mut check: impl FnMut() -> Result<(), E>) -> Result<Result<T, String>, E> {
            spin(|| self.0.ready.load(Ordering::Acquire));
            let mut s = lock(&self.0.state);
            loop {
                if let Some(r) = s.0.take() {
                    return Ok(r.map_err(|p| crate::unwind::panic_text(&*p)));
                }
                s = self.0.done.wait_timeout(s, POLL).unwrap_or_else(|e| e.into_inner()).0;
                if s.0.is_none() {
                    drop(s);
                    check()?;
                    s = lock(&self.0.state);
                }
            }
        }

        /// Waits for the worker result.
        pub(crate) fn wait_raw(self) -> std::thread::Result<T> {
            spin(|| self.0.ready.load(Ordering::Acquire));
            let mut s = lock(&self.0.state);
            loop {
                if let Some(r) = s.0.take() {
                    return r;
                }
                s = self.0.done.wait(s).unwrap_or_else(|e| e.into_inner());
            }
        }
    }

    #[cfg(feature = "os")]
    impl<T> std::future::Future for Pending<T> {
        type Output = Result<T, String>;

        fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
            let mut s = lock(&self.0.state);
            match s.0.take() {
                Some(r) => std::task::Poll::Ready(r.map_err(|p| crate::unwind::panic_text(&*p))),
                None => {
                    s.1 = Some(cx.waker().clone());
                    std::task::Poll::Pending
                }
            }
        }
    }
}

#[cfg(all(test, feature = "os"))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn results_panics_and_abandoned_jobs() {
        assert_eq!(offload(|| 6 * 7).wait(|| Ok::<_, ()>(())), Ok(Ok(42)));
        let r = offload(|| -> u8 { panic!("boom") }).wait(|| Ok::<_, ()>(()));
        assert_eq!(r, Ok(Err("boom".to_string())));
        let release = Arc::new(AtomicBool::new(false));
        let stuck = release.clone();
        let mut checks = 0;
        let r = offload(move || {
            while !stuck.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
        })
        .wait(|| {
            checks += 1;
            if checks < 3 { Ok(()) } else { Err("stop") }
        });
        assert_eq!(r, Err("stop"));
        assert_eq!(offload(|| 1).wait(|| Ok::<_, ()>(())), Ok(Ok(1)));
        release.store(true, Ordering::SeqCst);
    }

    #[test]
    fn deep_stack_jobs_and_their_panics() {
        let id = || std::thread::current().id();
        assert_ne!(on_deep_stack(id).unwrap(), id());
        assert!(on_deep_stack(move || on_deep_stack(id).unwrap() == id()).unwrap());
        let panic = on_deep_stack(|| -> u8 { panic!("boom") }).unwrap_err();
        assert_eq!(crate::unwind::panic_text(&*panic), "boom");
    }
}
