//! An unsynchronized broadcast channel with guaranteed delivery.
//!
//! This allocates storage internally to maintain shared state between the
//! [Sender] and [Receiver]s.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::fmt::Display;
use std::fmt::Formatter;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::broad_rc::{BroadRc, BroadWeak};

const DEFAULT_CAPACITY: usize = 16;

/// Error raised when trying to [Sender::try_send] but the subscribers either do
/// not have the necessary capacity or there are no subscribers.
///
/// The error includes the number of receivers that the value was delivered to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnderCapacity(pub usize);

impl Display for UnderCapacity {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "subscribers are under capacity")
    }
}

impl Error for UnderCapacity {}

struct ReceiverState<T> {
    /// Last message id received.
    id: u64,
    /// Waker to wake once receiving is available.
    waker: Option<Waker>,
    /// Test if the interior value is set.
    buf: VecDeque<T>,
    /// If the received is unbounded.
    unbounded: bool,
}

impl<T> ReceiverState<T> {
    /// Test if the current receiver is at capacity.
    fn at_capacity(&self) -> bool {
        !self.unbounded && self.buf.capacity() == self.buf.len()
    }
}

/// Interior shared state.
struct Shared<T> {
    /// The current message identifier.
    id: u64,
    /// Waker to wake once sending is available.
    sender: Option<Waker>,
    /// Collection of receivers.
    receivers: slab::Slab<ReceiverState<T>>,
    /// Per-subscriber capacity to use.
    capacity: Option<NonZeroUsize>,
}

/// Sender end of the channel created through [channel].
pub struct Sender<T>
where
    T: Clone,
{
    inner: BroadRc<Shared<T>>,
}

impl<T> Sender<T>
where
    T: Clone,
{
    /// Construct a new receiver and return its index in the slab of stored
    /// receivers.
    fn new_receiver(&mut self) -> usize {
        // Safety: Since this structure is single-threaded there is now way to
        // hold an inner reference at multiple locations.
        unsafe {
            let (inner, _) = self.inner.get_mut_unchecked();

            let (capacity, unbounded) = match inner.capacity {
                Some(capacity) => (capacity.get(), false),
                None => (DEFAULT_CAPACITY, true),
            };

            inner.receivers.insert(ReceiverState {
                id: inner.id,
                waker: None,
                buf: VecDeque::with_capacity(capacity),
                unbounded,
            })
        }
    }

    /// Bump the current message ID.
    fn bump_message_id(&mut self) {
        // Increase the ID of messages to send.
        unsafe {
            let (inner, _) = self.inner.get_mut_unchecked();

            inner.id = inner.id.wrapping_add(1);

            // Avoid 0, since that is what receivers are initialized to.
            if inner.id == 0 {
                inner.id = 1;
            }
        }
    }

    /// Subscribe to the broadcast channel.
    ///
    /// This will set up a new buffer for the returned [Receiver] which will
    /// allocate space for the number of elements specified when the channel was
    /// created with [channel].
    ///
    /// The returned [Receiver] is guaranteed to receive all updates to the
    /// current broadcast channel, even to the extend that sending to other
    /// receivers will be blocked. Note that this means that *slow receivers*
    /// are capable of hogging down the entire broadcast system since they must
    /// be delievered to (or dropped) in order for the system to make progress.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::broadcast;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let mut sender = broadcast::channel::<u32>(1);
    ///
    /// let mut sub1 = sender.subscribe();
    /// let mut sub2 = sender.subscribe();
    ///
    /// let (result, s1, s2) = tokio::join!(sender.send(42), sub1.recv(), sub2.recv());
    ///
    /// assert_eq!(result, 2);
    /// assert_eq!(s1, Some(42));
    /// assert_eq!(s2, Some(42));
    ///
    /// drop(sub1);
    ///
    /// let (result, s2) = tokio::join!(sender.send(84), sub2.recv());
    ///
    /// assert_eq!(result, 1);
    /// assert_eq!(s2, Some(84));
    ///
    /// drop(sub2);
    ///
    /// let result = sender.send(126).await;
    /// assert_eq!(result, 0);
    /// # }
    /// ```
    pub fn subscribe(&mut self) -> Receiver<T> {
        let index = self.new_receiver();

        Receiver {
            index,
            inner: self.inner.weak(),
        }
    }

    /// Get a count on the number of subscribers.
    pub fn subscribers(&self) -> usize {
        unsafe {
            let (inner, _) = self.inner.get_mut_unchecked();
            inner.receivers.len()
        }
    }

    /// Try to send a value to all subscribers in a non-blocking manner.
    ///
    /// This errors with [UnderCapacity] unless all subscribers have the
    /// capacity to receive the value.
    ///
    /// On an error, the subscribers that did have the capacity to receive the
    /// message will receive it.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::broadcast;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let mut tx = broadcast::channel::<u32>(1);
    /// let mut sub1 = tx.subscribe();
    /// let mut sub2 = tx.subscribe();
    ///
    /// assert_eq!(tx.try_send(1), Ok(2));
    /// assert_eq!(tx.try_send(2), Err(broadcast::UnderCapacity(0)));
    ///
    /// assert_eq!(sub2.recv().await, Some(1));
    /// assert_eq!(tx.try_send(3), Err(broadcast::UnderCapacity(1)));
    ///
    /// assert_eq!(sub2.recv().await, Some(3));
    /// # }
    /// ```
    pub fn try_send(&mut self, value: T) -> Result<usize, UnderCapacity> {
        self.bump_message_id();

        unsafe {
            let (inner, any_receivers_present) = self.inner.get_mut_unchecked();

            if !any_receivers_present {
                return Ok(0);
            }

            let mut delivered = 0;

            for (_, receiver) in &mut inner.receivers {
                // Receiver buffer is at capacity.
                if !receiver.at_capacity() {
                    delivered += 1;
                    receiver.buf.push_back(value.clone());

                    if let Some(waker) = &receiver.waker {
                        waker.wake_by_ref();
                    }
                }
            }

            if delivered == inner.receivers.len() {
                return Ok(delivered);
            }

            Err(UnderCapacity(delivered))
        }
    }

    /// Receive a message on the channel.
    ///
    /// Note that *not driving the returned future to completion* might result
    /// in some receivers not receiving the most up-to-date value.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::broadcast;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let mut sender = broadcast::channel::<u32>(1);
    ///
    /// let mut sub1 = sender.subscribe();
    /// let mut sub2 = sender.subscribe();
    ///
    /// let (result, s1, s2) = tokio::join!(sender.send(42), sub1.recv(), sub2.recv());
    ///
    /// assert_eq!(result, 2);
    /// assert_eq!(s1, Some(42));
    /// assert_eq!(s2, Some(42));
    ///
    /// drop(sub1);
    ///
    /// let (result, s2) = tokio::join!(sender.send(84), sub2.recv());
    ///
    /// assert_eq!(result, 1);
    /// assert_eq!(s2, Some(84));
    ///
    /// drop(sub2);
    ///
    /// let result = sender.send(126).await;
    /// assert_eq!(result, 0);
    /// # }
    /// ```
    pub async fn send(&mut self, value: T) -> usize {
        self.bump_message_id();

        Send {
            inner: &self.inner,
            value,
        }
        .await
    }
}

/// Future produced by [Sender::send].
struct Send<'a, T> {
    inner: &'a BroadRc<Shared<T>>,
    value: T,
}

impl<'a, T> Future for Send<'a, T>
where
    T: Clone,
{
    type Output = usize;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = Pin::into_inner(self);

        unsafe {
            let (inner, any_receivers_present) = this.inner.get_mut_unchecked();

            if !any_receivers_present {
                return Poll::Ready(0);
            }

            if !matches!(&inner.sender, Some(w) if w.will_wake(cx.waker())) {
                inner.sender = Some(cx.waker().clone());
            }

            loop {
                let mut any_sent = false;
                let mut delivered = 0;

                for (_, receiver) in &mut inner.receivers {
                    if receiver.id == inner.id {
                        delivered += 1;
                        continue;
                    }

                    // Receiver buffer is at capacity.
                    if receiver.at_capacity() {
                        continue;
                    }

                    receiver.buf.push_back(this.value.clone());

                    if let Some(waker) = &receiver.waker {
                        waker.wake_by_ref();
                    }

                    any_sent = true;
                }

                if delivered == inner.receivers.len() {
                    return Poll::Ready(delivered);
                }

                if any_sent {
                    continue;
                }

                return Poll::Pending;
            }
        }
    }
}

impl<'a, T> Unpin for Send<'a, T> {}

/// Receiver end of the channel created through [channel].
pub struct Receiver<T> {
    index: usize,
    inner: BroadWeak<Shared<T>>,
}

impl<T> Receiver<T> {
    /// Receive a message on the channel.
    ///
    /// Trying to receive a message on a queue that has been closed by dropping
    /// its [Sender] will result in `None` being returned.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::broadcast;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let mut sender = broadcast::channel::<u32>(1);
    ///
    /// let mut sub1 = sender.subscribe();
    /// let mut sub2 = sender.subscribe();
    ///
    /// let (result, s1, s2) = tokio::join!(sender.send(42), sub1.recv(), sub2.recv());
    ///
    /// assert_eq!(result, 2);
    /// assert_eq!(s1, Some(42));
    /// assert_eq!(s2, Some(42));
    ///
    /// drop(sender);
    ///
    /// let (s1, s2) = tokio::join!(sub1.recv(), sub2.recv());
    ///
    /// assert_eq!(s1, None);
    /// assert_eq!(s2, None);
    /// # }
    /// ```
    pub async fn recv(&mut self) -> Option<T> {
        Recv { receiver: self }.await
    }
}

/// Future associated with receiving through [Receiver::recv].
struct Recv<'a, T> {
    receiver: &'a mut Receiver<T>,
}

impl<'a, T> Future for Recv<'a, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = Pin::into_inner(self);
        unsafe {
            let index = this.receiver.index;
            let (inner, sender_present) = this.receiver.inner.get_mut_unchecked();

            let receiver = match inner.receivers.get_mut(index) {
                Some(receiver) => receiver,
                None => return Poll::Ready(None),
            };

            if let Some(value) = receiver.buf.pop_front() {
                receiver.id = inner.id;

                // Senders have interest once a buffer has been taken.
                if let Some(waker) = &inner.sender {
                    waker.wake_by_ref();
                }

                return Poll::Ready(Some(value));
            }

            if !sender_present {
                receiver.waker = None;
                return Poll::Ready(None);
            }

            if !matches!(&receiver.waker, Some(w) if !w.will_wake(cx.waker())) {
                receiver.waker = Some(cx.waker().clone())
            }

            if let Some(waker) = &inner.sender {
                waker.wake_by_ref();
            }

            Poll::Pending
        }
    }
}

impl<T> Drop for Recv<'_, T> {
    fn drop(&mut self) {
        unsafe {
            let index = self.receiver.index;
            let (inner, _) = self.receiver.inner.get_mut_unchecked();

            if let Some(receiver) = inner.receivers.get_mut(index) {
                receiver.buf.clear();
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
            let (inner, _) = self.inner.get_mut_unchecked();

            for (_, r) in &mut inner.receivers {
                if let Some(waker) = r.waker.take() {
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
            let (inner, _) = self.inner.get_mut_unchecked();
            let _ = inner.receivers.try_remove(index);

            if let Some(waker) = self.inner.get_mut_unchecked().0.sender.take() {
                waker.wake();
            }
        }
    }
}

/// Setup a broadcast channel with the given per-subscriber capacity.
///
/// # Panics
///
/// Panics if `capacity` is specified as 0.
pub fn channel<T>(capacity: usize) -> Sender<T>
where
    T: Clone,
{
    let capacity = NonZeroUsize::new(capacity).expect("capacity cannot be 0");

    let inner = BroadRc::new(Shared {
        id: 0,
        sender: None,
        receivers: slab::Slab::new(),
        capacity: Some(capacity),
    });

    Sender { inner }
}

/// Setup a broadcast channel which is unbounded.
///
/// # Panics
///
/// Panics if `capacity` is specified as 0.
pub fn unbounded<T>() -> Sender<T>
where
    T: Clone,
{
    let inner = BroadRc::new(Shared {
        id: 0,
        sender: None,
        receivers: slab::Slab::new(),
        capacity: None,
    });

    Sender { inner }
}
