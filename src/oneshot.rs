//! A oneshot `!Send` channel.
//!
//! This does allocate storage internally to maintain shared state between the
//! [Sender] and [Receiver].

use crate::bi_ref::BiRef;
use std::error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// Error raised when sending a message over the queue.
#[derive(Clone, Copy)]
#[non_exhaustive]
pub struct SendError<T>(pub T);

impl<T> fmt::Debug for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SendError").finish()
    }
}

impl<T> fmt::Display for SendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "channel disconnected")
    }
}

impl<T> error::Error for SendError<T> {}

/// Interior shared state.
struct Shared<T> {
    /// Waker to wake once sending is available.
    waker: Option<Waker>,
    /// Test if the interior value is set.
    buf: Option<T>,
}

/// Sender end of this queue.
#[repr(transparent)]
pub struct Sender<T> {
    inner: BiRef<Shared<T>>,
}

impl<T> Sender<T> {
    /// Receive a message on the channel.
    pub fn send(self, value: T) -> Result<(), SendError<T>> {
        unsafe {
            let (inner, both_present) = self.inner.get_mut_unchecked();

            if !both_present {
                return Err(SendError(value));
            }

            inner.buf = Some(value);

            if let Some(waker) = inner.waker.take() {
                waker.wake();
            }

            Ok(())
        }
    }
}

/// Receiver end of this queue.
pub struct Receiver<T> {
    inner: BiRef<Shared<T>>,
}

impl<T> Receiver<T> {
    /// Receive a message on the channel.
    pub fn recv(self) -> Recv<T> {
        Recv { receiver: self }
    }
}

pub struct Recv<T> {
    receiver: Receiver<T>,
}

impl<T> Future for Recv<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            let (inner, both_present) = this.receiver.inner.get_mut_unchecked();

            if let Some(value) = inner.buf.take() {
                return Poll::Ready(Some(value));
            }

            if !both_present {
                inner.waker = None;
                return Poll::Ready(None);
            }

            if !matches!(&inner.waker, Some(w) if w.will_wake(cx.waker())) {
                inner.waker = Some(cx.waker().clone());
            }

            Poll::Pending
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        unsafe {
            if let Some(waker) = self.inner.get_mut_unchecked().0.waker.take() {
                waker.wake();
            }
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        unsafe {
            if let Some(waker) = self.inner.get_mut_unchecked().0.waker.take() {
                waker.wake();
            }
        }
    }
}

/// Setup a spsc channel.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (a, b) = BiRef::new(Shared {
        waker: None,
        buf: None,
    });

    let rx = Receiver { inner: a };
    let tx = Sender { inner: b };
    (tx, rx)
}
