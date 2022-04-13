//! A dynamically-sized `!Send` broadcast channel.
//!
//! This does allocate storage internally to maintain shared state between the
//! [Sender] and [Receiver].

use crate::broad_ref::{BroadRef, Weak};
use std::error;
use std::fmt;
use std::future::Future;
use std::mem::replace;
use std::num::NonZeroU64;
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
        write!(f, "no receivers to broadcast channel")
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

struct ReceiverState<T> {
    /// Last message id received.
    id: Option<NonZeroU64>,
    /// Waker to wake once receiving is available.
    rx: Option<Waker>,
    /// Test if the interior value is set.
    state: State<T>,
}

impl<T> ReceiverState<T> {
    const EMPTY: Self = Self {
        id: None,
        rx: None,
        state: State::Empty,
    };
}

/// Interior shared state.
struct Shared<T> {
    /// The current message ID.
    id: u64,
    /// Waker to wake once sending is available.
    tx: Option<Waker>,
    /// Collection of receivers.
    receivers: slab::Slab<ReceiverState<T>>,
}

/// Sender end of this queue.
pub struct Sender<T>
where
    T: Clone,
{
    inner: BroadRef<Shared<T>>,
}

impl<T> Sender<T>
where
    T: Clone,
{
    fn next_index(&mut self) -> usize {
        // Safety: Since this structure is single-threaded there is now way to
        // hold an inner reference at multiple locations.
        unsafe {
            let (inner, _) = self.inner.load();
            inner.receivers.insert(ReceiverState::EMPTY)
        }
    }

    /// Construct a new receiver.
    pub fn subscribe(&mut self) -> Receiver<T> {
        let index = self.next_index();

        Receiver {
            index,
            inner: self.inner.weak(),
        }
    }

    /// Get a count on the number of subscribers.
    pub fn subscribers(&self) -> usize {
        unsafe {
            let (inner, _) = self.inner.load();
            inner.receivers.len()
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

struct Send<'a, T> {
    inner: &'a BroadRef<Shared<T>>,
    to_send: Option<T>,
}

impl<'a, T> Future for Send<'a, T>
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

                for (_, r) in &mut inner.receivers {
                    if matches!(r.id, Some(id) if id.get() == inner.id) {
                        count += 1;
                        continue;
                    }

                    match r.state {
                        State::Empty | State::Sending(..) => continue,
                        State::Waiting => (),
                    };

                    let to_send = this.to_send.as_ref().expect("future already completed");

                    // Write the data into the reference onto the stack of the waiting task.
                    dbg!(r.id);
                    r.state = State::Sending(to_send.clone());
                    r.id = NonZeroU64::new(inner.id);

                    if let Some(waker) = &r.rx {
                        waker.wake_by_ref();
                    };

                    any_sent = true;
                }

                if count == inner.receivers.len() {
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
pub struct Receiver<T> {
    index: usize,
    inner: Weak<Shared<T>>,
}

impl<T> Receiver<T> {
    /// Receive a message on the channel.
    pub async fn recv(&mut self) -> Option<T> {
        unsafe {
            let (inner, both_present) = self.inner.load();

            if !both_present {
                return None;
            }

            let receiver = inner.receivers.get_mut(self.index)?;

            println!("set as waiting");
            receiver.state = State::Waiting;
            Recv { receiver: self }.await
        }
    }
}

struct Recv<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<'a, T> Future for Recv<'a, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            let index = this.receiver.index;
            let (inner, both_present) = this.receiver.inner.load();

            let receiver = match inner.receivers.get_mut(index) {
                Some(receiver) => receiver,
                None => return Poll::Ready(None),
            };

            if let State::Sending(..) = receiver.state {
                if let State::Sending(value) = replace(&mut receiver.state, State::Empty) {
                    return Poll::Ready(Some(value));
                }
            }

            if !both_present {
                receiver.rx = None;
                return Poll::Ready(None);
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

impl<T> Drop for Recv<'_, T> {
    fn drop(&mut self) {
        println!("dropped receiver");

        unsafe {
            let index = self.receiver.index;
            let (inner, _) = self.receiver.inner.load();

            if let Some(receiver) = inner.receivers.get_mut(index) {
                receiver.state = State::Empty;
            }
        }
    }
}

impl<T> Drop for Sender<T>
where
    T: Clone,
{
    fn drop(&mut self) {
        unsafe {
            let (inner, _) = self.inner.load();

            for (_, r) in &mut inner.receivers {
                if let Some(waker) = r.rx.take() {
                    waker.wake();
                }
            }
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        unsafe {
            let index = self.index;
            let (inner, _) = self.inner.load();
            let _ = inner.receivers.try_remove(index);

            if let Some(waker) = self.inner.load().0.tx.take() {
                waker.wake();
            }
        }
    }
}

/// Setup a broadcast channel.
pub fn channel<T>() -> Sender<T>
where
    T: Clone,
{
    let inner = BroadRef::new(Shared {
        id: 0,
        tx: None,
        receivers: slab::Slab::new(),
    });

    Sender { inner }
}
