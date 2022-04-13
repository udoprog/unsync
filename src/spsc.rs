//! An unsynchronized single-producer, single-consumer channel.
//!
//! You might also know this simply as a "queue", but I'm sticking with a
//! uniform naming scheme here so give me a break.
//!
//! This allocates storage internally to maintain shared state between the
//! [Sender] and [Receiver].

use std::collections::VecDeque;
use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::bi_rc::BiRc;

/// Error raised when sending a message over the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SendError<T>(pub T);

impl<T> Display for SendError<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "channel disconnected")
    }
}

impl<T> Error for SendError<T> where T: Debug {}

/// Interior shared state.
///
/// Note that we maintain two sets of waker to avoid having to clone the waker
/// associated with the channel unecessarily through the [Waker::will_wake]
/// optimization. This is done because it's presumed that the channel will be
/// re-used.
struct Shared<T> {
    /// Waker to wake once sending is available.
    tx: Option<Waker>,
    /// Waker to wake once receiving is available.
    rx: Option<Waker>,
    /// Test if the interior value is set.
    buf: VecDeque<T>,
    /// Indicates if the channel is unbounded.
    unbounded: bool,
}

impl<T> Shared<T> {
    /// Test if the current channel is at capacity.
    fn at_capacity(&self) -> bool {
        !self.unbounded && self.buf.capacity() == self.buf.len()
    }
}

/// Sender end of the channel created through [channel].
pub struct Sender<T> {
    inner: BiRc<Shared<T>>,
}

impl<T> Sender<T> {
    /// Try to send a message on this channel without blocking.
    ///
    /// This will succeed if there is sufficient capacity to send, but fail
    /// otherwise.
    ///
    /// Note: don't attempt to use this as an optimization over [Sender::send]
    /// since it already performs this operation internally as needed.
    ///
    /// # Examples
    ///
    /// ```rust
    /// #[tokio::main(flavor = "current_thread")]
    /// # async fn main() {
    /// let (mut tx, mut rx) = unsync::spsc::channel(3);
    /// assert!(tx.try_send(1).is_ok());
    /// assert!(tx.try_send(2).is_ok());
    /// assert!(tx.try_send(3).is_ok());
    /// assert!(tx.try_send(4).is_err());
    ///
    /// let first = rx.recv().await;
    /// assert_eq!(first, Some(1));
    ///
    /// assert!(tx.try_send(5).is_ok());
    /// assert!(tx.try_send(6).is_err());
    ///
    /// let mut collected = Vec::new();
    ///
    /// // Drop sender so that the channel "ends".
    /// drop(tx);
    ///
    /// while let Some(value) = rx.recv().await {
    ///     collected.push(value);
    /// }
    ///
    /// assert_eq!(collected, vec![2, 3, 5]);
    /// # }
    /// ```
    pub fn try_send(&mut self, value: T) -> Result<(), SendError<T>> {
        unsafe {
            let (inner, both_present) = self.inner.get_mut_unchecked();

            if !both_present || inner.at_capacity() {
                return Err(SendError(value));
            }

            inner.buf.push_back(value);

            if let Some(waker) = &inner.rx {
                waker.wake_by_ref();
            };

            Ok(())
        }
    }

    /// Send a message on this channel.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::task;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<(), task::JoinError> {
    /// let (mut tx, mut rx) = unsync::spsc::channel(1);
    ///
    /// let local = task::LocalSet::new();
    ///
    /// let collected = local.run_until(async move {
    ///     let collect = task::spawn_local(async move {
    ///         let mut out = Vec::new();
    ///
    ///         while let Some(value) = rx.recv().await {
    ///             out.push(value);
    ///         }
    ///
    ///         out
    ///     });
    ///
    ///     let sender = task::spawn_local(async move {
    ///         for n in 0..10 {
    ///             let result = tx.send(n).await;
    ///         }
    ///     });
    ///
    ///     collect.await
    /// }).await?;
    ///
    /// assert_eq!(collected, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    /// # Ok(()) }
    /// ```
    pub async fn send(&mut self, value: T) -> Result<(), SendError<T>> {
        Send {
            inner: &self.inner,
            value: Some(value),
        }
        .await
    }
}

/// Future returned when sending a value through [Sender::send].
struct Send<'a, T> {
    inner: &'a BiRc<Shared<T>>,
    value: Option<T>,
}

impl<'a, T> Future for Send<'a, T> {
    type Output = Result<(), SendError<T>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);

            let (inner, both_present) = this.inner.get_mut_unchecked();

            if !both_present {
                inner.tx = None;
                let value = this.value.take().expect("future already completed");
                return Poll::Ready(Err(SendError(value)));
            }

            // If we are at capacity, register ourselves as an interested waker
            // and move on.
            if inner.at_capacity() {
                if !matches!(&inner.tx, Some(w) if w.will_wake(cx.waker())) {
                    inner.tx = Some(cx.waker().clone());
                }

                return Poll::Pending;
            };

            inner
                .buf
                .push_back(this.value.take().expect("future already completed"));

            if let Some(waker) = &inner.rx {
                waker.wake_by_ref();
            };

            Poll::Ready(Ok(()))
        }
    }
}

/// Receiver end of the channel created through [channel].
pub struct Receiver<T> {
    inner: BiRc<Shared<T>>,
}

impl<T> Receiver<T> {
    /// Receive a message on this channel.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use tokio::task;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<(), task::JoinError> {
    /// let (mut tx, mut rx) = unsync::spsc::channel(1);
    ///
    /// let local = task::LocalSet::new();
    ///
    /// let collected = local.run_until(async move {
    ///     let collect = task::spawn_local(async move {
    ///         let mut out = Vec::new();
    ///
    ///         while let Some(value) = rx.recv().await {
    ///             out.push(value);
    ///         }
    ///
    ///         out
    ///     });
    ///
    ///     let sender = task::spawn_local(async move {
    ///         for n in 0..10 {
    ///             let result = tx.send(n).await;
    ///         }
    ///     });
    ///
    ///     collect.await
    /// }).await?;
    ///
    /// assert_eq!(collected, vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
    /// # Ok(()) }
    /// ```
    pub async fn recv(&mut self) -> Option<T> {
        Recv { inner: &self.inner }.await
    }
}

struct Recv<'a, T> {
    inner: &'a BiRc<Shared<T>>,
}

impl<'a, T> Future for Recv<'a, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe {
            let this = Pin::get_unchecked_mut(self);
            let (inner, both_present) = this.inner.get_mut_unchecked();

            if let Some(value) = inner.buf.pop_front() {
                return Poll::Ready(Some(value));
            }

            if !both_present {
                inner.rx = None;
                return Poll::Ready(None);
            }

            if !matches!(&inner.rx, Some(w) if !w.will_wake(cx.waker())) {
                inner.rx = Some(cx.waker().clone())
            }

            if let Some(tx) = &inner.tx {
                tx.wake_by_ref();
            }

            Poll::Pending
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        unsafe {
            if let Some(waker) = self.inner.get_mut_unchecked().0.rx.take() {
                waker.wake();
            }
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        unsafe {
            if let Some(waker) = self.inner.get_mut_unchecked().0.tx.take() {
                waker.wake();
            }
        }
    }
}

/// Setup a spsc with the given capacity.
///
/// Any sender is capable of sending without blocking up until `capacity` number
/// of elements have been buffered.
///
/// # Panics
///
/// Panics if `capacity` is set to `0`.
pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>) {
    assert!(capacity > 0, "capacity cannot be 0");

    let (a, b) = BiRc::new(Shared {
        tx: None,
        rx: None,
        buf: VecDeque::with_capacity(capacity),
        unbounded: false,
    });

    let rx = Receiver { inner: a };
    let tx = Sender { inner: b };

    (tx, rx)
}

/// Setup a spsc with an unbounded capacity.
///
/// Sending through this channel will never block.
pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let (a, b) = BiRc::new(Shared {
        tx: None,
        rx: None,
        buf: VecDeque::new(),
        unbounded: true,
    });

    let rx = Receiver { inner: a };
    let tx = Sender { inner: b };

    (tx, rx)
}
