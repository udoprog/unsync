//! A fixed-size `!Send` broadcast channel.
//!
//! You might also know this simply as a "queue", but I'm sticking with a
//! uniform naming scheme.
//!
//! This uses no buffer for messages sent, instead it relies on the sender
//! allocating the value being received on the stack.
//!
//! It does however allocate internally in order to communicate flexibly between
//! the [Sender] and [Receiver] halves.

use crate::broad_ref::{BroadRef, Weak};
use std::error;
use std::fmt;
use std::future::Future;
use std::mem::replace;
use std::mem::MaybeUninit;
use std::num::NonZeroU64;
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
        write!(f, "no receivers to broadcast channel")
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

struct ReceiverState<T> {
    /// If the receiver is active.
    active: bool,
    /// Last message id received.
    id: Option<NonZeroU64>,
    /// Waker to wake once receiving is available.
    rx: Option<Waker>,
    /// Test if the interior value is set.
    state: State<T>,
}

impl<T> ReceiverState<T> {
    const EMPTY: Self = Self {
        active: false,
        id: None,
        rx: None,
        state: State::Empty,
    };
}

/// Interior shared state.
struct Shared<const N: usize, T> {
    /// The current message ID.
    id: u64,
    /// Waker to wake once sending is available.
    tx: Option<Waker>,
    /// Collection of receivers.
    receivers: [ReceiverState<T>; N],
}

/// Sender end of this queue.
pub struct Sender<const N: usize, T>
where
    T: Clone,
{
    inner: BroadRef<Shared<N, T>>,
}

impl<const N: usize, T> Sender<N, T>
where
    T: Clone,
{
    fn next_index(&mut self) -> Option<usize> {
        // Safety: Since this structure is single-threaded there is now way to
        // hold an inner reference at multiple locations.
        unsafe {
            let (inner, _) = self.inner.load();

            for (index, receiver) in inner.receivers.iter_mut().enumerate() {
                if !receiver.active {
                    receiver.active = true;
                    return Some(index);
                }
            }

            None
        }
    }

    /// Construct a new receiver.
    pub fn subscribe(&mut self) -> Option<Receiver<N, T>> {
        let index = self.next_index()?;

        Some(Receiver {
            index,
            inner: self.inner.weak(),
        })
    }

    /// Get a count on the number of subscribers.
    pub fn subscribers(&self) -> usize {
        unsafe {
            let (inner, _) = self.inner.load();
            let mut count = 0;

            for receiver in &mut inner.receivers {
                if receiver.active {
                    count += 1;
                }
            }

            count
        }
    }

    /// Receive a message on the channel.
    pub async fn send(&mut self, value: T) -> Result<(), SendError<T>> {
        // Increase the ID of messages to send.
        unsafe {
            let (inner, _) = self.inner.load();
            inner.id = inner.id.wrapping_add(1);

            if inner.id == 0 {
                inner.id = 1;
            }
        }

        Send {
            inner: &self.inner,
            to_send: Some(value),
        }
        .await
    }
}

struct Send<'a, const N: usize, T> {
    inner: &'a BroadRef<Shared<N, T>>,
    to_send: Option<T>,
}

impl<'a, const N: usize, T> Future for Send<'a, N, T>
where
    T: Clone,
{
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);

            let (inner, both_present) = this.inner.load();

            if !both_present {
                let value = this.to_send.take().expect("future already completed");
                return Poll::Ready(Err(SendError(value)));
            }

            if !matches!(&inner.tx, Some(w) if w.will_wake(cx.waker())) {
                inner.tx = Some(cx.waker().clone());
            }

            loop {
                let mut any_sent = false;
                let mut count = 0;

                for r in &mut inner.receivers {
                    if !r.active {
                        count += 1;
                        continue;
                    }

                    if matches!(r.id, Some(id) if id.get() == inner.id) {
                        count += 1;
                        continue;
                    }

                    let mut value = match r.state {
                        State::Empty | State::Sending(..) => continue,
                        State::Waiting(data) => data,
                    };

                    let to_send = this.to_send.as_ref().expect("future already completed");

                    // Write the data into the reference onto the stack of the waiting task.
                    value.as_mut().write(to_send.clone());
                    r.state = State::Sending(value);
                    r.id = NonZeroU64::new(inner.id);

                    if let Some(waker) = &r.rx {
                        waker.wake_by_ref();
                    };

                    any_sent = true;
                }

                if count == N {
                    return Poll::Ready(Ok(()));
                }

                if any_sent {
                    continue;
                }

                return Poll::Pending;
            }
        }
    }
}

/// Receiver end of this queue.
pub struct Receiver<const N: usize, T> {
    index: usize,
    inner: Weak<Shared<N, T>>,
}

impl<const N: usize, T> Receiver<N, T> {
    /// Receive a message on the channel.
    pub async fn recv(&mut self) -> Option<T> {
        unsafe {
            let mut value = MaybeUninit::uninit();

            let (inner, both_present) = self.inner.load();

            if !both_present {
                return None;
            }

            let receiver = inner.receivers.get_mut(self.index)?;
            receiver.state = State::Waiting(NonNull::from(&mut value));
            let recv = Recv { receiver: self };

            if recv.await {
                Some(value.assume_init())
            } else {
                None
            }
        }
    }
}

struct Recv<'a, const N: usize, T> {
    receiver: &'a Receiver<N, T>,
}

impl<'a, const N: usize, T> Future for Recv<'a, N, T> {
    type Output = bool;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            let index = this.receiver.index;
            let (inner, both_present) = this.receiver.inner.load();

            let receiver = match inner.receivers.get_mut(index) {
                Some(receiver) => receiver,
                None => return Poll::Ready(false),
            };

            if let State::Sending(..) = receiver.state {
                receiver.state = State::Empty;
                return Poll::Ready(true);
            }

            if !both_present {
                receiver.rx = None;
                return Poll::Ready(false);
            }

            if !matches!(&receiver.rx, Some(w) if !w.will_wake(cx.waker())) {
                receiver.rx = Some(cx.waker().clone())
            }

            if let Some(waker) = &inner.tx {
                waker.wake_by_ref();
            }

            Poll::Pending
        }
    }
}

impl<const N: usize, T> Drop for Recv<'_, N, T> {
    fn drop(&mut self) {
        unsafe {
            let index = self.receiver.index;
            let (inner, _) = self.receiver.inner.load();

            if let Some(receiver) = inner.receivers.get_mut(index) {
                // Need to drop the value which is in the process of being sent,
                // because this process will not receive it.
                if let State::Sending(value) = replace(&mut receiver.state, State::Empty) {
                    ptr::drop_in_place((*value.as_ptr()).as_mut_ptr());
                }

                receiver.state = State::Empty;
            }
        }
    }
}

impl<const N: usize, T> Drop for Sender<N, T>
where
    T: Clone,
{
    fn drop(&mut self) {
        unsafe {
            let (inner, _) = self.inner.load();

            for r in &mut inner.receivers {
                if let Some(waker) = r.rx.take() {
                    waker.wake();
                }
            }
        }
    }
}

impl<const N: usize, T> Drop for Receiver<N, T> {
    fn drop(&mut self) {
        unsafe {
            let index = self.index;
            let (inner, _) = self.inner.load();

            if let Some(receiver) = inner.receivers.get_mut(index) {
                receiver.active = false;
            }

            if let Some(waker) = self.inner.load().0.tx.take() {
                waker.wake();
            }
        }
    }
}

/// Setup a broadcast channel.
pub fn channel<const N: usize, T>() -> Sender<N, T>
where
    T: Clone,
{
    let inner = BroadRef::new(Shared {
        id: 0,
        tx: None,
        receivers: [ReceiverState::EMPTY; N],
    });

    Sender { inner }
}
