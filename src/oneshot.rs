//! A oneshot `!Send` channel.
//!
//! This uses no buffer for messages sent, instead it relies on the [Receiver]
//! allocating the value being received on the stack. This does however mean
//! that [Receiver] cannot implement [Future] directly, but must use a
//! [Receiver::recv] method.
//!
//! It does however allocate internally in order to communicate flexibly between
//! the [Sender] and [Receiver] halves.

use crate::bi_ref::BiRef;
use std::error;
use std::fmt;
use std::future::Future;
use std::mem::replace;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::ptr;
use std::ptr::NonNull;
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
    Waiting(NonNull<MaybeUninit<T>>),
    /// A message has been written to the channel and is waiting to be received.
    Sending(NonNull<MaybeUninit<T>>),
}

/// Interior shared state.
struct Shared<T> {
    /// Waker to wake once sending is available.
    waker: Option<Waker>,
    /// Test if the interior value is set.
    state: State<T>,
}

/// Sender end of this queue.
#[repr(transparent)]
pub struct Sender<T> {
    inner: BiRef<Shared<T>>,
}

impl<T> Sender<T> {
    /// Receive a message on the channel.
    pub async fn send(self, value: T) -> Result<(), SendError<T>> {
        Send {
            sender: self,
            to_send: Some(value),
        }
        .await
    }
}

struct Send<T> {
    sender: Sender<T>,
    to_send: Option<T>,
}

impl<T> Future for Send<T> {
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);

            let (inner, both_present) = this.sender.inner.load();

            if !both_present {
                let value = this.to_send.take().expect("future already completed");
                return Poll::Ready(Err(SendError(value)));
            }

            let mut value = match inner.state {
                State::Empty => {
                    // Waker is now expected to be set by *this side*.
                    if !matches!(&inner.waker, Some(w) if w.will_wake(cx.waker())) {
                        inner.waker = Some(cx.waker().clone());
                    }

                    return Poll::Pending;
                }
                State::Sending(..) => {
                    // Waker is exclusively managed by the other side, so wake
                    // them up!
                    if let Some(waker) = inner.waker.take() {
                        waker.wake();
                    }

                    return Poll::Pending;
                }
                State::Waiting(data) => data,
            };

            let to_send = this.to_send.take().expect("future already completed");

            // Write the data into the reference onto the stack of the waiting task.
            value.as_mut().write(to_send);
            inner.state = State::Sending(value);

            // Wake up the other side to receive the value.
            if let Some(waker) = inner.waker.take() {
                waker.wake();
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
    pub async fn recv(self) -> Option<T> {
        unsafe {
            let mut value = MaybeUninit::uninit();

            let (inner, both_present) = self.inner.load();

            if !both_present {
                return None;
            }

            inner.state = State::Waiting(NonNull::from(&mut value));

            // The other end is waiting to be woken up, so let's wake it up!
            if let Some(waker) = inner.waker.take() {
                waker.wake();
            }

            if Recv(&self.inner).await {
                Some(value.assume_init())
            } else {
                None
            }
        }
    }
}

struct Recv<'a, T>(&'a BiRef<Shared<T>>);

impl<'a, T> Future for Recv<'a, T> {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            let (inner, both_present) = this.0.load();

            if let State::Sending(..) = inner.state {
                inner.state = State::Empty;
                return Poll::Ready(true);
            }

            if !both_present {
                inner.waker = None;
                return Poll::Ready(false);
            }

            // At this point we are guaranteed to be waiting, so we can only
            // reach this spot if we woke up for some reason. Ensure that the
            // wake is set.
            if !matches!(&inner.waker, Some(w) if !w.will_wake(cx.waker())) {
                inner.waker = Some(cx.waker().clone())
            }

            Poll::Pending
        }
    }
}

impl<T> Drop for Recv<'_, T> {
    fn drop(&mut self) {
        unsafe {
            let (inner, _) = self.0.load();

            // Need to drop the value which is in the process of being sent,
            // because this process will not receive it.
            if let State::Sending(value) = replace(&mut inner.state, State::Empty) {
                inner.state = State::Empty;
                ptr::drop_in_place((*value.as_ptr()).as_mut_ptr());
            }
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        unsafe {
            if let Some(waker) = self.inner.load().0.waker.take() {
                waker.wake();
            }
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        unsafe {
            if let Some(waker) = self.inner.load().0.waker.take() {
                waker.wake();
            }
        }
    }
}

/// Setup a spsc channel.
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (a, b) = BiRef::new(Shared {
        waker: None,
        state: State::Empty,
    });

    let rx = Receiver { inner: a };
    let tx = Sender { inner: b };
    (tx, rx)
}
