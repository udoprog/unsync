//! An unsynchronized oneshot channel.
//!
//! This allocates storage internally to maintain shared state between the
//! [Sender] and [Receiver].

use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use crate::bi_rc::BiRc;

/// Error raised when sending a message over the queue.
///
/// # Examples
///
/// ```
/// use unsync::oneshot;
///
/// # fn main() {
/// let (tx, rx) = oneshot::channel::<u32>();
/// drop(rx);
/// assert_eq!(tx.send(1), Err(oneshot::SendError(1)));
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SendError<T>(pub T);

impl<T> Display for SendError<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "channel disconnected")
    }
}

impl<T> Error for SendError<T> where T: Debug {}

/// Interior shared state.
struct Shared<T> {
    /// Waker to wake once value is set.
    waker: Option<Waker>,
    /// Test if the interior value is set.
    buf: Option<T>,
}

/// Sender end of the channel created through [channel].
///
/// See [Sender::send] for how to use.
pub struct Sender<T> {
    inner: BiRc<Shared<T>>,
}

impl<T> Sender<T> {
    /// Send a message over this channel.
    ///
    /// # Errors
    ///
    /// This function raises [SendError] in case the receiver end of the channel
    /// has been closed.
    ///
    /// ```
    /// use unsync::oneshot;
    ///
    /// # fn main() {
    /// let (tx, rx) = oneshot::channel::<u32>();
    /// drop(rx);
    /// assert_eq!(tx.send(1), Err(oneshot::SendError(1)));
    /// # }
    /// ```
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::oneshot;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let (tx, rx) = oneshot::channel();
    /// assert!(tx.send(1).is_ok());
    /// assert_eq!(rx.await, Some(1));
    /// # }
    /// ```
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

/// Receiver end of the channel created through [channel].
///
/// This implements [Future] so that it can be awaited or polled directly.
///
/// # Examples
///
/// ```
/// use unsync::oneshot;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let (tx, rx) = oneshot::channel();
/// assert!(tx.send(1).is_ok());
/// assert_eq!(rx.await, Some(1));
/// # }
/// ```
pub struct Receiver<T> {
    inner: BiRc<Shared<T>>,
}

impl<T> Future for Receiver<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = Pin::into_inner(self);
        unsafe {
            let (inner, both_present) = this.inner.get_mut_unchecked();

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

/// Setup a oneshot channel.
///
/// # Examples
///
/// ```
/// use unsync::oneshot;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let (tx, rx) = oneshot::channel();
/// assert!(tx.send(1).is_ok());
/// assert_eq!(rx.await, Some(1));
/// # }
/// ```
pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let (a, b) = BiRc::new(Shared {
        waker: None,
        buf: None,
    });

    let rx = Receiver { inner: a };
    let tx = Sender { inner: b };
    (tx, rx)
}
