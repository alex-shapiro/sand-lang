//! Host-to-Sand entry experiments on unpatched Sand APIs:
//! E17, an event that the host sends from outside any native, mixed with sync entries;
//! E18, a session driven by a hand-written run loop with no tokio runtime.
use sandlang::engine::{Config, Limits};
use sandlang::session::Session;
use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

fn poll_once<F: Future>(f: F) -> Option<F::Output> {
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}

fn config() -> Config {
    Config::new().limits(Limits { max_instructions: None, max_cpu_time: None, max_wall_time: None, ..Limits::default() })
}

async fn e17() {
    let mut s = Session::new(config());
    s.eval("var started = 0\nvar taps = 0\nvar plain = 0\nfn on_tap() { started += 1\n task.sleep(1ms)\n taps += 1 }\nfn on_plain() { plain += 1 }\nfn on_layout(w: int) -> int { w / 2 }").await.unwrap();
    // The host sends events: it can only evaluate code that spawns the handlers.
    s.eval("spawn on_tap()\nspawn on_plain()").await.unwrap();
    std::thread::sleep(Duration::from_millis(5));
    let a = s.eval("\"started \\(started), taps \\(taps), plain \\(plain)\"").await;
    std::thread::sleep(Duration::from_millis(5));
    let b = s.eval("\"started \\(started), taps \\(taps), plain \\(plain)\"").await;
    println!("E17a after the events, two later evals: {a:?}; {b:?}");
    s.run_tasks_until(tokio::time::sleep(Duration::from_millis(20)), |e| println!("task error: {e}")).await;
    println!("E17b after the host drives run_tasks_until for 20 ms: {:?}", s.eval("\"started \\(started), taps \\(taps)\"").await);
    // A sync entry while a handler is queued: does the sync entry run part of the handler?
    s.eval("spawn on_tap()").await.unwrap();
    let sync = poll_once(s.eval("\"layout \\(on_layout(640)), started \\(started), taps \\(taps)\""));
    println!("E17c sync entry right after a host event: {sync:?}");
    s.run_tasks_until(tokio::time::sleep(Duration::from_millis(20)), |e| println!("task error: {e}")).await;
    println!("E17d after run_tasks_until: {:?}", s.eval("\"started \\(started), taps \\(taps)\"").await);
    // A suspended handler can also resume inside a later eval that waits, or that computes for more than 2 ms.
    s.eval("spawn on_tap()").await.unwrap();
    let e = s.eval("\"started \\(started), taps \\(taps)\"").await;
    let f = s.eval("task.sleep(5ms)\n\"started \\(started), taps \\(taps)\"").await;
    println!("E17e new event, then a short eval: {e:?}; then an eval that sleeps 5 ms: {f:?}");
    s.eval("spawn on_tap()").await.unwrap();
    let _ = s.eval("\"started \\(started)\"").await;
    std::thread::sleep(Duration::from_millis(3));
    let g = s.eval("var a = 0\nfor i in 0..3000000 { a += i % 7 }\n\"started \\(started), taps \\(taps)\"").await;
    println!("E17f new event, then an eval that computes for more than 2 ms: {g:?}");
}

/// A minimal run loop: a thread-parking waker and no tokio runtime.
fn block_on_plain<F: Future>(f: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) { self.0.unpark(); }
    }
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut f = std::pin::pin!(f);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut Context::from_waker(&waker)) {
            return v;
        }
        if Instant::now() > deadline {
            panic!("no progress within 5 s");
        }
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

fn e18() {
    let mut s = Session::new(config());
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block_on_plain(s.eval("1 + 1"))));
    println!("E18a compute-only eval on a plain run loop, no tokio runtime: {:?}", r.map(|x| x.map_err(|e| e.to_string())));
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block_on_plain(s.eval("task.sleep(1ms)\n5"))));
    println!("E18b eval that sleeps on a plain run loop, no tokio runtime: {:?}", r.map(|x| x.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string())).map_err(|_| "panic"));
    // With a tokio runtime entered (its timer driven by the runtime's own thread), the same plain loop.
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_time().build().unwrap();
    let _guard = rt.enter();
    let mut s = Session::new(config());
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| block_on_plain(s.eval("task.sleep(1ms)\n5"))));
    println!("E18c eval that sleeps on a plain run loop, with a tokio runtime entered on another thread: {:?}", r.map(|x| x.map_err(|e| e.to_string().lines().next().unwrap_or("").to_string())).map_err(|_| "panic"));
}

fn main() {
    println!("== sandlang host entry experiments");
    e18();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(e17());
}
