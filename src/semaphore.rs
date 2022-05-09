//! [`Semaphore`] provides an unsychronized asynchronous semaphore for permit acquisition.

use std::cell::Cell;
use std::mem::ManuallyDrop;

use crate::wait_list::WaitList;

/// An asynchronous semaphore for permit acquisition.
///
/// A semaphore allows limiting access to a shared resource to a certain number
/// of callers at a time. It is created with a certain number of _permits_ which
/// can be shared among tasks, and tasks can wait for permits to become
/// available. Semaphores are commonly used for rate limiting.
///
/// This semaphore supports both fair and unfair operations. There are two
/// aspects of fairness to consider:
///
/// 1. Whether a task that wants fewer permits can obtain those permits while a
///     task that wants more permits waits. The default [`acquire`] method does
///     not allow this, but [`acquire_unfair`] does.
/// 2. Whether a task can steal the permits from another task if that second
///    task has not been scheduled yet (so the permits have been released but
///    have yet to be acquired). This kind of unfairness improves throughput for
///    tasks that rapidly release and acquire permits without yielding, so by
///    default it is used. However if permits are released with
///    [`release_fair`], they will be directly and fairly handed off to the
///    first waiter in line, disallowing any [`acquire`] call from
///    opportunistically taking them.
///
/// In comparison to [Tokio's semaphore], this semaphore:
/// - Is `!Sync` (obviously).
/// - Does not support closing.
/// - Tracks the total number of permits as well as the current number of
///   available permits.
/// - Does not place a limit on the total number of permits — you can go up to
///   `usize::MAX`.
/// - Consistently uses `usize` to count permit numbers instead of using `u32`
///   sometimes.
/// - Gives more control over the fairness algorithms used.
///
/// [`acquire`]: Self::acquire
/// [`acquire_unfair`]: Self::acquire_unfair
/// [`release_fair`]: Permit::release_fair
/// [Tokio's semaphore]: https://docs.rs/tokio/1/tokio/sync/struct.Semaphore.html
#[derive(Debug)]
pub struct Semaphore {
    /// List of waiters.
    ///
    /// Each waiter contains a `usize` that stores the number of permits desired.
    waiters: WaitList<usize, WakeUp>,

    /// The number of available permits in the semaphore.
    permits: Cell<usize>,

    /// The total number of permits in the semaphore.
    total_permits: Cell<usize>,
}

/// Ways in which a waiter can be woken.
#[derive(Clone, Copy)]
enum WakeUp {
    /// The waiter was fairly given the number of permits it requested by `add_permits_fair`.
    Fair,
    /// The waiter was notified by `add_permits` that the requested number of permits may be
    /// available, but wasn't given them directly.
    Unfair,
}

impl Semaphore {
    /// Create a new semaphore with the given number of permits.
    #[must_use]
    pub const fn new(permits: usize) -> Self {
        Self {
            waiters: WaitList::new(),
            permits: Cell::new(permits),
            total_permits: Cell::new(permits),
        }
    }

    /// Retrieve the number of currently available permits.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::Semaphore;
    ///
    /// let semaphore = Semaphore::new(100);
    /// assert_eq!(semaphore.available_permits(), 100);
    ///
    /// semaphore.add_permits(10);
    /// assert_eq!(semaphore.available_permits(), 110);
    ///
    /// let guard = semaphore.try_acquire(20).unwrap();
    /// assert_eq!(semaphore.available_permits(), 90);
    /// assert_eq!(guard.permits(), 20);
    ///
    /// drop(guard);
    /// assert_eq!(semaphore.available_permits(), 110);
    ///
    /// semaphore.try_acquire(20).unwrap().leak();
    /// assert_eq!(semaphore.available_permits(), 90);
    /// ```
    #[must_use]
    pub fn available_permits(&self) -> usize {
        self.permits.get()
    }

    /// Retrieve the total number of permits, including those currently handed
    /// out.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::Semaphore;
    ///
    /// let semaphore = Semaphore::new(100);
    /// assert_eq!(semaphore.total_permits(), 100);
    ///
    /// semaphore.add_permits(10);
    /// assert_eq!(semaphore.total_permits(), 110);
    ///
    /// let guard = semaphore.try_acquire(20).unwrap();
    /// assert_eq!(semaphore.total_permits(), 110);
    /// assert_eq!(guard.permits(), 20);
    ///
    /// drop(guard);
    /// assert_eq!(semaphore.total_permits(), 110);
    ///
    /// semaphore.try_acquire(20).unwrap().leak();
    /// assert_eq!(semaphore.total_permits(), 90);
    /// ```
    #[must_use]
    pub fn total_permits(&self) -> usize {
        self.total_permits.get()
    }

    /// Add additional permits to the semaphore.
    ///
    /// # Panics
    ///
    /// This function will panic if it would result in more than `usize::MAX` total permits.
    pub fn add_permits(&self, new_permits: usize) {
        self.total_permits.set(
            self.total_permits
                .get()
                .checked_add(new_permits)
                .expect("number of permits overflowed"),
        );

        self.release_permits(new_permits, WakeUp::Unfair);
    }

    /// Add new permits to the semaphore, using a fair wakeup algorithm to
    /// ensure that the new permits won't be taken by any waiter other than the
    /// one at the front of the queue.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::Semaphore;
    ///
    /// let s = Semaphore::new(3);
    /// let permit = s.try_acquire(2);
    /// assert!(s.try_acquire(2).is_none());
    /// s.add_permits_fair(2);
    /// assert!(s.try_acquire(2).is_some());
    /// ```
    ///
    /// # Panics
    ///
    /// This function will panic if it would result in more than [`usize::MAX`]
    /// total permits.
    pub fn add_permits_fair(&self, new_permits: usize) {
        self.total_permits
            .set(self.total_permits.get().checked_add(new_permits).unwrap());
        self.release_permits(new_permits, WakeUp::Fair);
    }

    /// Attempt to acquire permits from the semaphore immediately.
    ///
    /// [`None`] is returned if there are not enough permits available **or** a
    /// task is currently waiting for a permit.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::Semaphore;
    ///
    /// let s = Semaphore::new(3);
    /// let permit = s.try_acquire(2);
    /// assert!(s.try_acquire(2).is_none());
    /// drop(permit);
    /// assert!(s.try_acquire(2).is_some());
    /// ```
    ///
    pub fn try_acquire(&self, to_acquire: usize) -> Option<Permit<'_>> {
        // If a task is already waiting for some permits, we mustn't steal it.
        if !self.waiters.borrow().is_empty() {
            println!("not empty");
            return None;
        }

        self.try_acquire_unfair(to_acquire)
    }

    /// Attempt to acquire permits from the semaphore immediately, potentially
    /// unfairly stealing permits from a task that is waiting for permits.
    ///
    /// [`None`] is returned if there are not enough permits available, but
    /// **not** if a task is currently waiting for a permit.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::future::Future;
    /// use std::task::Poll;
    ///
    /// use unsync::Semaphore;
    /// # let cx = &mut unsync::utils::noop_cx();
    ///
    /// let semaphore = Semaphore::new(1);
    ///
    /// let mut future = Box::pin(semaphore.acquire(2));
    /// assert!(future.as_mut().poll(cx).is_pending());
    ///
    /// assert!(semaphore.try_acquire(1).is_none());
    /// assert!(semaphore.try_acquire_unfair(1).is_some());
    ///
    /// drop(future);
    ///
    /// assert!(semaphore.try_acquire(1).is_some());
    /// ```
    pub fn try_acquire_unfair(&self, to_acquire: usize) -> Option<Permit<'_>> {
        let new_permits = self.permits.get().checked_sub(to_acquire)?;
        self.permits.set(new_permits);

        Some(Permit {
            semaphore: self,
            permits: to_acquire,
        })
    }

    /// Acquire permits from the semaphore.
    ///
    /// # Examples
    ///
    /// The following showcases the default use of a permit.
    ///
    /// ```
    /// use unsync::Semaphore;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let semaphore = Semaphore::new(1);
    /// let mut permit = semaphore.acquire(1).await;
    /// assert!(semaphore.try_acquire(1).is_none());
    /// # }
    /// ```
    pub async fn acquire(&self, to_acquire: usize) -> Permit<'_> {
        loop {
            if let Some(guard) = self.try_acquire(to_acquire) {
                break guard;
            }

            match self.waiters.wait(to_acquire).await {
                WakeUp::Unfair => continue,
                WakeUp::Fair => {
                    return Permit {
                        semaphore: self,
                        permits: to_acquire,
                    };
                }
            }
        }
    }

    /// Acquire permits from the semaphore, potentially unfairly stealing
    /// permits from a task that is waiting for permits.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::future::Future;
    ///
    /// use unsync::Semaphore;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// # let cx = &mut unsync::utils::noop_cx();
    /// let semaphore = Semaphore::new(1);
    ///
    /// {
    ///     let future = semaphore.acquire(2);
    ///     tokio::pin!(future);
    ///     assert!(future.as_mut().poll(cx).is_pending());
    ///
    ///     assert!(semaphore.try_acquire(1).is_none());
    ///     // steal one permit from `future` which is in the process of acquiring permits.
    ///     let permit = semaphore.acquire_unfair(1).await;
    ///     drop(permit);
    /// }
    ///
    /// // Since `future` is dropped here we can now acquire more permits fairly.
    /// assert!(semaphore.try_acquire(1).is_some());
    /// # }
    /// ```
    pub async fn acquire_unfair(&self, to_acquire: usize) -> Permit<'_> {
        loop {
            if let Some(guard) = self.try_acquire_unfair(to_acquire) {
                break guard;
            }

            match self.waiters.wait(to_acquire).await {
                WakeUp::Unfair => continue,
                WakeUp::Fair => {
                    return Permit {
                        semaphore: self,
                        permits: to_acquire,
                    };
                }
            }
        }
    }

    fn release_permits(&self, permits: usize, fairness: WakeUp) {
        let mut permits = self.permits.get() + permits;
        self.permits.set(permits);

        let mut waiters = self.waiters.borrow();

        while let Some(&wanted_permits) = waiters.head_input() {
            permits = match permits.checked_sub(wanted_permits) {
                Some(new_permits) => new_permits,
                None => break,
            };

            if let WakeUp::Fair = fairness {
                self.permits.set(permits);
            }

            if waiters.wake_one(fairness).is_err() {
                // Hint that the `None` branch can be optimized away. We know
                // this is unreachable since we've cheat that an head input
                // exists above.
                unreachable!();
            }
        }
    }
}

/// A RAII guard holding a number of permits obtained from a [`Semaphore`].
///
/// # Examples
///
/// The following showcases the default use of a permit.
///
/// ```
/// use std::future::Future;
/// use std::task::Poll;
///
/// use unsync::Semaphore;
/// # let cx = &mut unsync::utils::noop_cx();
///
/// let semaphore = Semaphore::new(1);
///
/// let initial = semaphore.try_acquire(1).unwrap();
///
/// let mut f1 = Box::pin(semaphore.acquire(1));
/// assert!(f1.as_mut().poll(cx).is_pending());
///
/// drop(initial);
///
/// let mut f2 = Box::pin(semaphore.acquire(1));
/// assert!(f2.as_mut().poll(cx).is_ready());
/// assert!(f1.as_mut().poll(cx).is_ready());
/// ```
#[derive(Debug)]
pub struct Permit<'semaphore> {
    semaphore: &'semaphore Semaphore,
    permits: usize,
}

impl<'semaphore> Permit<'semaphore> {
    /// Retrieve a shared reference to the semaphore this guard is for.
    #[must_use]
    pub fn semaphore(&self) -> &'semaphore Semaphore {
        self.semaphore
    }

    /// Get the number of permits this guard holds.
    ///
    /// ```
    /// use unsync::Semaphore;
    ///
    /// let semaphore = Semaphore::new(100);
    ///
    /// let guard = semaphore.try_acquire(20).unwrap();
    /// assert_eq!(semaphore.available_permits(), 80);
    /// assert_eq!(semaphore.total_permits(), 100);
    /// assert_eq!(guard.permits(), 20);
    /// drop(guard);
    ///
    /// assert_eq!(semaphore.available_permits(), 100);
    /// assert_eq!(semaphore.total_permits(), 100);
    /// ```
    #[must_use]
    pub fn permits(&self) -> usize {
        self.permits
    }

    /// Leak the permits without releasing it to the semaphore.
    ///
    /// This reduces the total number of permits in the semaphore.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::Semaphore;
    ///
    /// let semaphore = Semaphore::new(100);
    /// assert_eq!(semaphore.available_permits(), 100);
    /// assert_eq!(semaphore.total_permits(), 100);
    ///
    /// semaphore.try_acquire(20).unwrap().leak();
    /// assert_eq!(semaphore.available_permits(), 80);
    /// assert_eq!(semaphore.total_permits(), 80);
    /// ```
    pub fn leak(self) {
        let this = ManuallyDrop::new(self);
        let reduced_permits = this.semaphore.total_permits.get() - this.permits;
        this.semaphore.total_permits.set(reduced_permits);
    }

    /// Release the permits using a fair wakeup algorithm to ensure that the new
    /// permits won't be taken by any waiter other than the one at the front of
    /// the queue.
    ///
    /// To release the permits with an unfair wakeup algorithm, simply call the
    /// [`drop`] on this value or have it fall out of scope.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::future::Future;
    /// use std::task::Poll;
    ///
    /// use unsync::Semaphore;
    /// # let cx = &mut unsync::utils::noop_cx();
    ///
    /// let semaphore = Semaphore::new(1);
    ///
    /// let initial = semaphore.try_acquire(1).unwrap();
    ///
    /// let mut f1 = Box::pin(semaphore.acquire(1));
    /// assert!(f1.as_mut().poll(cx).is_pending());
    ///
    /// initial.release_fair();
    ///
    /// let mut f2 = Box::pin(semaphore.acquire(1));
    /// assert!(f2.as_mut().poll(cx).is_pending());
    /// assert!(f1.as_mut().poll(cx).is_ready());
    /// assert!(f2.as_mut().poll(cx).is_ready());
    /// ```
    pub fn release_fair(self) {
        let this = ManuallyDrop::new(self);
        this.semaphore.release_permits(this.permits, WakeUp::Fair);
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.semaphore()
            .release_permits(self.permits(), WakeUp::Unfair);
    }
}
