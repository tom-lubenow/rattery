//! Background work that does not block the UI.
//!
//! The app is single threaded and async, so a server call can run while the
//! event loop keeps handling input. [`spawn`] starts a future and returns a
//! [`Task`] handle; when the future finishes, the next call to
//! [`event::next`](crate::event::next) yields [`Event::Wake`](crate::event::Event::Wake)
//! so the app re-renders and picks up the result.
//!
//! ```ignore
//! let mut pending = Some(rattery_app::task::spawn(fetch_snapshot()));
//! loop {
//!     terminal.draw(|f| ui(f, pending.is_some()))?;
//!     match rattery_app::event::next().await {
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

use futures::future::{AbortHandle, Abortable};

struct Slot<T> {
    value: RefCell<Option<T>>,
    done: Cell<bool>,
    waker: RefCell<Option<Waker>>,
}

/// A handle to a spawned future. Dropping it cancels the work; call
/// [`Task::detach`] to let it run to completion unobserved.
pub struct Task<T> {
    slot: Rc<Slot<T>>,
    abort: AbortHandle,
    detached: bool,
}

/// Run `future` in the background. Must be called from inside an app.
pub fn spawn<F>(future: F) -> Task<F::Output>
where
    F: Future + 'static,
    F::Output: 'static,
{
    let slot = Rc::new(Slot {
        value: RefCell::new(None),
        done: Cell::new(false),
        waker: RefCell::new(None),
    });
    let (abort, registration) = AbortHandle::new_pair();
    let work = Abortable::new(future, registration);
    wit_bindgen::spawn_local({
        let slot = slot.clone();
        async move {
            if let Ok(value) = work.await {
                *slot.value.borrow_mut() = Some(value);
                slot.done.set(true);
                if let Some(waker) = slot.waker.borrow_mut().take() {
                    waker.wake();
                }
                wake();
            }
        }
    });
    Task {
        slot,
        abort,
        detached: false,
    }
}

impl<T> Task<T> {
    /// True once the future has produced a value (whether or not it was taken).
    pub fn is_done(&self) -> bool {
        self.slot.done.get()
    }

    /// True while the future is still running.
    pub fn is_pending(&self) -> bool {
        !self.slot.done.get()
    }

    /// Take the result if the future has finished. Returns `None` while pending
    /// and after the value was already taken.
    pub fn try_take(&mut self) -> Option<T> {
        self.slot.value.borrow_mut().take()
    }

    /// Let the future keep running after this handle is dropped.
    pub fn detach(mut self) {
        self.detached = true;
    }
}

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        if !self.detached {
            self.abort.abort();
        }
    }
}

impl<T> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        if let Some(value) = self.slot.value.borrow_mut().take() {
            return Poll::Ready(value);
        }
        assert!(
            !self.slot.done.get(),
            "rattery_app::task::Task polled after its value was taken"
        );
        *self.slot.waker.borrow_mut() = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl<T> std::fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Task")
            .field("done", &self.slot.done.get())
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
