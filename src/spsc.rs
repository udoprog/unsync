//! A single-producer, single-consumer `!Send` channel.
//!
//! You might also know this simply as a "queue", but I'm sticking with a
//! uniform naming scheme.
//!
//! This does allocate storage internally to maintain shared state between the
//! [Sender] and [Receiver].

use crate::bi_ref::BiRef;
use std::error;
use std::fmt;
use std::future::Future;
use std::mem::replace;
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

#[derive(Debug, Clone, Copy)]
enum State<T> {
    /// Channel is empty.
    Empty,
    /// Channel is in the process of waiting for a message.
    Waiting,
    /// A message has been written to the channel and is waiting to be received.
    Sending(T),
}

/// Interior shared state.
struct Shared<T> {
    /// Waker to wake once sending is available.
    tx: Option<Waker>,
    /// Waker to wake once receiving is available.
    rx: Option<Waker>,
    /// Test if the interior value is set.
    state: State<T>,
}

/// Sender end of this queue.
pub struct Sender<T> {
    inner: BiRef<Shared<T>>,
}

impl<T> Sender<T> {
    /// Receive a message on the channel.
    pub async fn send(&mut self, value: T) -> Result<(), SendError<T>> {
        Send {
            inner: &self.inner,
            to_send: Some(value),
        }
        .await
    }
}

struct Send<'a, T> {
    inner: &'a BiRef<Shared<T>>,
    to_send: Option<T>,
}

impl<'a, T> Future for Send<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);

            let (inner, both_present) = this.inner.load();

            if !both_present {
                let value = this.to_send.take().expect("future already completed");
                return Poll::Ready(Err(SendError(value)));
            }

            match &inner.state {
                State::Empty | State::Sending(..) => {
                    if !matches!(&inner.tx, Some(w) if w.will_wake(cx.waker())) {
                        inner.tx = Some(cx.waker().clone());
                    }

                    return Poll::Pending;
                }
                State::Waiting => (),
            };

            let to_send = this.to_send.take().expect("future already completed");
            inner.state = State::Sending(to_send);

            if let Some(waker) = &inner.rx {
                waker.wake_by_ref();
            };

            Poll::Ready(Ok(()))
        }
    }
}

/// Receiver end of this queue.
pub struct Receiver<T> {
    inner: BiRef<Shared<T>>,
}

impl<T> Receiver<T> {
    /// Receive a message on the channel.
    pub async fn recv(&mut self) -> Option<T> {
        unsafe {
            let (inner, both_present) = self.inner.load();

            if !both_present {
                return None;
            }

            inner.state = State::Waiting;
            Recv(&self.inner).await
        }
    }
}

struct Recv<'a, T>(&'a BiRef<Shared<T>>);

impl<'a, T> Future for Recv<'a, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            let (inner, both_present) = this.0.load();

            if let State::Sending(..) = &inner.state {
                if let State::Sending(value) = replace(&mut inner.state, State::Empty) {
                    return Poll::Ready(Some(value));
                }
            }

            if !both_present {
                inner.rx = None;
                return Poll::Ready(None);
            }

            if !matches!(&inner.rx, Some(w) if !w.will_wake(cx.waker())) {
                inner.rx = Some(cx.waker().clone())
            }

            if let Some(waker) = &inner.tx {
                waker.wake_by_ref();
            }

            Poll::Pending
        }
    }
}

impl<T> Drop for Recv<'_, T> {
    fn drop(&mut self) {
        unsafe {
            let (inner, _) = self.0.load();
            inner.state = State::Empty;
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        unsafe {
            if let Some(waker) = self.inner.load().0.rx.take() {
                waker.wake();
            }
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        unsafe {
            if let Some(waker) = self.inner.load().0.tx.take() {
                waker.wake();
            }
        }
    }
}

/// Setup a spsc channel.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (a, b) = BiRef::new(Shared {
        tx: None,
        rx: None,
        state: State::Empty,
    });

    let rx = Receiver { inner: a };
    let tx = Sender { inner: b };

    (tx, rx)
}
