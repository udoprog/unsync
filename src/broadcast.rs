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

    /// Send a message to every subscriber, returning the number it reached.
    ///
    /// This completes once the value has been buffered for each subscriber. If a
    /// subscriber's buffer is full it waits until that subscriber receives a
    /// value and frees a slot.
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

impl<T> Future for Send<'_, T>
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

            let mut delivered = 0;

            for (_, receiver) in &mut inner.receivers {
                // Already holds this message.
                if receiver.id == inner.id {
                    delivered += 1;
                    continue;
                }

                // No room yet; the receiver wakes us when it pops a value.
                if receiver.at_capacity() {
                    continue;
                }

                receiver.buf.push_back(this.value.clone());
                receiver.id = inner.id;

                if let Some(waker) = &receiver.waker {
                    waker.wake_by_ref();
                }

                delivered += 1;
            }

            if delivered == inner.receivers.len() {
                return Poll::Ready(delivered);
            }

            Poll::Pending
        }
    }
}

impl<T> Unpin for Send<'_, T> {}

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

impl<T> Future for Recv<'_, T> {
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
                // Popping freed a slot, so wake a sender waiting for room.
                if let Some(waker) = &inner.sender {
                    waker.wake_by_ref();
                }

                return Poll::Ready(Some(value));
            }

            if !sender_present {
                receiver.waker = None;
                return Poll::Ready(None);
            }

            if !matches!(&receiver.waker, Some(w) if w.will_wake(cx.waker())) {
                receiver.waker = Some(cx.waker().clone())
            }

            if let Some(waker) = &inner.sender {
                waker.wake_by_ref();
            }

            Poll::Pending
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

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};

    use super::channel;
    use crate::utils::noop_cx;

    struct Flag(AtomicBool);

    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    // A `recv` re-polled with a new waker (e.g. moved between tasks) must register
    // it. Otherwise a later send wakes the old waker and the receiver never runs.
    #[test]
    fn recv_updates_changed_waker() {
        let mut tx = channel::<u32>(1);
        let mut sub = tx.subscribe();

        let first = Arc::new(Flag(AtomicBool::new(false)));
        let second = Arc::new(Flag(AtomicBool::new(false)));
        let w1 = Waker::from(first.clone());
        let w2 = Waker::from(second.clone());

        let mut fut = Box::pin(sub.recv());
        assert!(fut.as_mut().poll(&mut Context::from_waker(&w1)).is_pending());
        assert!(fut.as_mut().poll(&mut Context::from_waker(&w2)).is_pending());

        assert_eq!(tx.try_send(1), Ok(1));

        assert!(second.0.load(Ordering::SeqCst), "latest waker was not woken");
    }

    // A buffer of capacity > 1 must hold several distinct messages and hand them
    // back in order, without losing any.
    #[test]
    fn buffers_multiple_messages_in_order() {
        let cx = &mut noop_cx();
        let mut tx = channel::<u32>(2);
        let mut sub = tx.subscribe();

        assert_eq!(tx.try_send(1), Ok(1));
        assert_eq!(tx.try_send(2), Ok(1));

        let v1 = {
            let mut r = Box::pin(sub.recv());
            r.as_mut().poll(cx)
        };
        let v2 = {
            let mut r = Box::pin(sub.recv());
            r.as_mut().poll(cx)
        };
        assert_eq!(v1, Poll::Ready(Some(1)));
        assert_eq!(v2, Poll::Ready(Some(2)));
    }

    // A single `send` buffers exactly one copy and resolves once buffered, with
    // no second copy left behind.
    #[test]
    fn send_buffers_once_and_resolves() {
        let cx = &mut noop_cx();
        let mut tx = channel::<u32>(2);
        let mut sub = tx.subscribe();

        let mut s = Box::pin(tx.send(1));
        assert_eq!(s.as_mut().poll(cx), Poll::Ready(1));
        drop(s);

        let v1 = {
            let mut r = Box::pin(sub.recv());
            r.as_mut().poll(cx)
        };
        let v2 = {
            let mut r = Box::pin(sub.recv());
            r.as_mut().poll(cx)
        };
        assert_eq!(v1, Poll::Ready(Some(1)));
        assert_eq!(v2, Poll::Pending);
    }

    // A full subscriber buffer makes `send` wait until the subscriber pops a
    // value, then resume.
    #[test]
    fn send_waits_for_capacity() {
        let cx = &mut noop_cx();
        let mut tx = channel::<u32>(1);
        let mut sub = tx.subscribe();

        let mut s1 = Box::pin(tx.send(1));
        assert_eq!(s1.as_mut().poll(cx), Poll::Ready(1));
        drop(s1);

        // The only slot is taken, so this send parks.
        let mut s2 = Box::pin(tx.send(2));
        assert!(s2.as_mut().poll(cx).is_pending());

        // Receiving frees the slot, so the send can finish.
        let v1 = {
            let mut r = Box::pin(sub.recv());
            r.as_mut().poll(cx)
        };
        assert_eq!(v1, Poll::Ready(Some(1)));
        assert_eq!(s2.as_mut().poll(cx), Poll::Ready(1));

        let v2 = {
            let mut r = Box::pin(sub.recv());
            r.as_mut().poll(cx)
        };
        assert_eq!(v2, Poll::Ready(Some(2)));
    }
}
