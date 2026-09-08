//! Background work that does not block the UI.
//!
//! The app is single threaded and async, so a server call can run while the
//! event loop keeps handling input. [`spawn`] starts a future and returns a
//! [`Task`] handle; when the future finishes, the next call to
//! [`event::next`](crate::event::next) yields [`Event::Wake`](crate::event::Event::Wake)
//! so the app re-renders and picks up the result.
//!
//! ```ignore
//! let mut pending = Some(rattery::task::spawn(fetch_snapshot()));
//! loop {
//!     terminal.draw(|f| ui(f, pending.is_some()))?;
//!     match rattery::event::next().await {
//!         Event::Wake => {
//!             if let Some(result) = pending.as_mut().and_then(Task::try_take) {
//!                 pending = None;
//!                 apply(result);
//!             }
//!         }
//!         // ...
//!     }
//! }
//! ```

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// A handle to a spawned future. Dropping it cancels the work; call
/// [`Task::detach`] to let it run to completion unobserved.
pub struct Task<T> {
    slot: Rc<RefCell<Option<T>>>,
    done: Rc<Cell<bool>>,
    inner: Option<wstd::runtime::Task<()>>,
}

/// Run `future` in the background. Must be called from inside [`crate::run`].
pub fn spawn<F>(future: F) -> Task<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let slot = Rc::new(RefCell::new(None));
    let done = Rc::new(Cell::new(false));
    let inner = wstd::runtime::spawn({
        let slot = slot.clone();
        let done = done.clone();
        async move {
            let value = future.await;
            *slot.borrow_mut() = Some(value);
            done.set(true);
            wake();
        }
    });
    Task {
        slot,
        done,
        inner: Some(inner),
    }
}

impl<T> Task<T> {
    /// True once the future has produced a value (whether or not it was taken).
    pub fn is_done(&self) -> bool {
        self.done.get()
    }

    /// True while the future is still running.
    pub fn is_pending(&self) -> bool {
        !self.done.get()
    }

    /// Take the result if the future has finished. Returns `None` while pending
    /// and after the value was already taken.
    pub fn try_take(&mut self) -> Option<T> {
        self.slot.borrow_mut().take()
    }

    /// Let the future keep running after this handle is dropped.
    pub fn detach(mut self) {
        if let Some(inner) = self.inner.take() {
            inner.detach();
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        if let Some(value) = self.slot.borrow_mut().take() {
            return Poll::Ready(value);
        }
        let inner = self
            .inner
            .as_mut()
            .expect("rattery::task::Task polled after completion");
        match Pin::new(inner).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(()) => Poll::Ready(
                self.slot
                    .borrow_mut()
                    .take()
                    .expect("task finished without a value"),
            ),
        }
    }
}

impl<T> std::fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Task")
            .field("done", &self.done.get())
            .finish()
    }
}

#[derive(Default)]
struct WakeState {
    pending: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

thread_local! {
    static WAKE: WakeState = WakeState::default();
}

/// Make the next [`event::next`](crate::event::next) return
/// [`Event::Wake`](crate::event::Event::Wake). Spawned tasks call this when
/// they finish; call it yourself from a task that wants the UI to redraw
/// before it is done, for example after each chunk of a streaming response.
pub fn wake() {
    WAKE.with(|w| {
        w.pending.set(true);
        if let Some(waker) = w.waker.borrow_mut().take() {
            waker.wake();
        }
    });
}

/// Consume a pending wake, if any.
pub(crate) fn take_wake() -> bool {
    WAKE.with(|w| w.pending.replace(false))
}

/// Resolves the next time [`wake`] is called.
pub(crate) fn woken() -> impl Future<Output = ()> {
    std::future::poll_fn(|cx| {
        WAKE.with(|w| {
            if w.pending.replace(false) {
                Poll::Ready(())
            } else {
                *w.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
    })
}
