//! Defence in depth against bugs: a Rust panic reachable from a script (in
//! the VM, a native or an I/O future) must not unwind into the host. The
//! engine's entry points run their futures inside [`CatchUnwind`], and the
//! scheduler runs each slice of the interpreter (with the natives it calls)
//! and each I/O finisher through [`catch`] (once per slice, so that native
//! calls cost nothing extra). A panic becomes an error (`Error::Runtime`, or
//! a panic of the run) whose message starts with `internal error:`.
//!
//! This only helps when panics unwind (the default); with `panic = "abort"`
//! the process still aborts. The default panic hook still prints the panic
//! to standard error.

use std::any::Any;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::task::{Context, Poll};

/// The text of a panic payload.
pub(crate) fn panic_text(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

/// Runs `f`, turning a panic into `Err(message)`.
pub(crate) fn catch<R>(f: impl FnOnce() -> R) -> Result<R, String> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|p| panic_text(&*p))
}

/// A future that resolves to `Err(message)` if polling `F` panics. The
/// inner future is dropped after a panic and never polled again.
pub(crate) struct CatchUnwind<F: Future> {
    inner: Option<Pin<Box<F>>>,
}

impl<F: Future> CatchUnwind<F> {
    pub(crate) fn new(f: F) -> Self {
        CatchUnwind { inner: Some(Box::pin(f)) }
    }
}

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, String>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let Some(inner) = self.inner.as_mut() else { return Poll::Ready(Err("polled after a panic".into())) };
        match catch(|| inner.as_mut().poll(cx)) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(v)) => {
                self.inner = None;
                Poll::Ready(Ok(v))
            }
            Err(msg) => {
                // Dropping a future that panicked may panic again.
                let inner = self.inner.take();
                let _ = catch(move || drop(inner));
                Poll::Ready(Err(msg))
            }
        }
    }
}

/// Runs the engine future `f`, reporting a panic as an internal error.
pub(crate) async fn guard<T>(f: impl Future<Output = Result<T, crate::Error>>) -> Result<T, crate::Error> {
    match CatchUnwind::new(f).await {
        Ok(r) => r,
        Err(msg) => Err(crate::Error::Runtime { message: format!("internal error: {msg}"), trace: vec![] }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panics_become_errors() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let r: Result<i32, crate::Error> = rt.block_on(guard(async {
            tokio::task::yield_now().await;
            panic!("boom {}", 1)
        }));
        assert!(matches!(r, Err(crate::Error::Runtime { ref message, .. }) if message == "internal error: boom 1"), "{r:?}");
        assert_eq!(rt.block_on(guard(async { Ok::<_, crate::Error>(5) })).unwrap(), 5);
        assert_eq!(catch(|| panic!("static")), Err::<(), _>("static".to_string()));
    }
}
