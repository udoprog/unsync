//! [`Semaphore`] provides an unsychronized asynchronous semaphore for permit acquisition.

use std::cell::Cell;
use std::mem;
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
///    task that wants more permits waits. The default [`acquire`] method does
///    not allow this, but [`acquire_unfair`] does.
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

/// How permits are released to waiters.
#[derive(Debug, Clone, Copy)]
enum Fairness {
    /// Deducted up front and handed straight to the woken waiter, so no-one else
    /// can take them.
    Fair,
    /// Just made available; woken waiters re-contend for them via `try_acquire`.
    Unfair,
}

/// The value handed to a woken waiter through the wait list.
enum WakeUp {
    /// Permits may be available; the waiter must re-contend via `try_acquire`.
    /// The token passes the wakeup on if this waiter is cancelled before doing so.
    Unfair(UnfairWoken),
    /// The requested permits were granted directly. The grant holds them until
    /// the waiter turns it into a `Permit`.
    Fair(FairGrant),
}

/// Handed to a waiter woken by an unfair release so it can contend for permits.
/// If the waiter is cancelled before contending, this `Drop` re-runs the wake so
/// the permits are offered to the next waiter rather than left stranded.
struct UnfairWoken {
    semaphore: *const Semaphore,
}

impl Drop for UnfairWoken {
    fn drop(&mut self) {
        // SAFETY: the semaphore outlives every waiter holding a token.
        let semaphore = unsafe { &*self.semaphore };
        semaphore.release_permits(0, Fairness::Unfair);
    }
}

/// Permits reserved for a specific waiter by a fair release. If the waiter
/// resumes it forgets the grant and turns it into a `Permit`; if it is cancelled
/// first, this `Drop` hands the permits back to the semaphore instead of leaking
/// them.
struct FairGrant {
    semaphore: *const Semaphore,
    permits: usize,
}

impl Drop for FairGrant {
    fn drop(&mut self) {
        // SAFETY: the semaphore outlives every waiter holding a grant.
        let semaphore = unsafe { &*self.semaphore };
        semaphore.release_permits(self.permits, Fairness::Unfair);
    }
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

        self.release_permits(new_permits, Fairness::Unfair);
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
        self.release_permits(new_permits, Fairness::Fair);
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
                WakeUp::Unfair(token) => {
                    // We were woken from the head of the queue, so take the
                    // permits without deferring to waiters still queued behind us
                    // (plain `try_acquire` would refuse while the queue is
                    // non-empty). On success forget the token; otherwise let it
                    // drop, re-waking the next waiter so the wakeup isn't lost.
                    if let Some(guard) = self.try_acquire_unfair(to_acquire) {
                        mem::forget(token);
                        break guard;
                    }
                }
                WakeUp::Fair(grant) => {
                    // The `Permit` below owns these permits now; forget the grant
                    // so they aren't released twice.
                    mem::forget(grant);
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
                WakeUp::Unfair(token) => {
                    // Already at the head; take the permits now. On success
                    // forget the token, otherwise let it drop and re-wake the
                    // next waiter so the wakeup isn't lost.
                    if let Some(guard) = self.try_acquire_unfair(to_acquire) {
                        mem::forget(token);
                        break guard;
                    }
                }
                WakeUp::Fair(grant) => {
                    // The `Permit` below owns these permits now; forget the grant
                    // so they aren't released twice.
                    mem::forget(grant);
                    return Permit {
                        semaphore: self,
                        permits: to_acquire,
                    };
                }
            }
        }
    }

    fn release_permits(&self, permits: usize, fairness: Fairness) {
        let mut permits = self.permits.get() + permits;
        self.permits.set(permits);

        let mut waiters = self.waiters.borrow();

        while let Some(&wanted_permits) = waiters.head_input() {
            permits = match permits.checked_sub(wanted_permits) {
                Some(new_permits) => new_permits,
                None => break,
            };

            let wake = match fairness {
                Fairness::Fair => {
                    // Commit the deduction; the grant returns the permits if the
                    // waiter is cancelled before it claims them.
                    self.permits.set(permits);
                    WakeUp::Fair(FairGrant {
                        semaphore: self,
                        permits: wanted_permits,
                    })
                }
                Fairness::Unfair => WakeUp::Unfair(UnfairWoken { semaphore: self }),
            };

            if waiters.wake_one(wake).is_err() {
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
        this.semaphore.release_permits(this.permits, Fairness::Fair);
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.semaphore()
            .release_permits(self.permits(), Fairness::Unfair);
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::Semaphore;
    use crate::utils::noop_cx;

    // A fair release reserves the permit for `f1`. If `f1` is dropped before
    // claiming it, the permit must go back to the semaphore rather than leak.
    #[test]
    fn fair_grant_to_cancelled_waiter_is_not_leaked() {
        let cx = &mut noop_cx();
        let sem = Semaphore::new(1);

        let initial = sem.try_acquire(1).unwrap();
        assert_eq!(sem.available_permits(), 0);

        let mut f1 = Box::pin(sem.acquire(1));
        assert!(f1.as_mut().poll(cx).is_pending());

        initial.release_fair();
        assert_eq!(sem.available_permits(), 0);

        drop(f1);

        assert_eq!(sem.available_permits(), 1);
        assert!(sem.try_acquire(1).is_some());
    }

    // The permits returned by a cancelled fair waiter must be able to wake the
    // next waiter queued behind it (FairGrant::drop re-enters release_permits).
    #[test]
    fn permits_from_cancelled_fair_waiter_wake_next_waiter() {
        let cx = &mut noop_cx();
        let sem = Semaphore::new(1);

        let initial = sem.try_acquire(1).unwrap();
        let mut f1 = Box::pin(sem.acquire(1));
        let mut f2 = Box::pin(sem.acquire(1));
        assert!(f1.as_mut().poll(cx).is_pending());
        assert!(f2.as_mut().poll(cx).is_pending());

        initial.release_fair();
        assert_eq!(sem.available_permits(), 0);

        // `f1`'s reserved permit must flow to `f2`.
        drop(f1);
        assert!(f2.as_mut().poll(cx).is_ready());
    }

    // Releasing one permit while two tasks wait must let the first waiter take
    // it. The just-woken front waiter must not defer to the task queued behind
    // it (which would leave both parked and the permit unclaimed).
    #[test]
    fn release_wakes_front_waiter_with_another_queued() {
        let cx = &mut noop_cx();
        let sem = Semaphore::new(1);
        let initial = sem.try_acquire(1).unwrap();

        let mut f1 = Box::pin(sem.acquire(1));
        let mut f2 = Box::pin(sem.acquire(1));
        assert!(f1.as_mut().poll(cx).is_pending());
        assert!(f2.as_mut().poll(cx).is_pending());

        drop(initial);
        assert!(f1.as_mut().poll(cx).is_ready());
    }

    // Cancelling a woken waiter must not strand the permit it was woken for; the
    // next waiter in line must still receive it.
    #[test]
    fn cancelled_woken_waiter_does_not_strand_permit() {
        let cx = &mut noop_cx();
        let sem = Semaphore::new(1);
        let initial = sem.try_acquire(1).unwrap();

        let mut w1 = Box::pin(sem.acquire(1));
        let mut w2 = Box::pin(sem.acquire(1));
        assert!(w1.as_mut().poll(cx).is_pending());
        assert!(w2.as_mut().poll(cx).is_pending());

        // Releasing one permit wakes the front waiter, `w1`.
        drop(initial);

        // `w1` is cancelled before it can take the permit.
        drop(w1);

        // The permit must flow to `w2`, not be lost.
        assert!(w2.as_mut().poll(cx).is_ready());
    }

    // The re-woken permit must reach a still-queued waiter even when several are
    // waiting behind the cancelled one.
    #[test]
    fn cancelled_woken_waiter_rewakes_across_queue() {
        let cx = &mut noop_cx();
        let sem = Semaphore::new(2);
        let initial = sem.try_acquire(2).unwrap();

        let mut w1 = Box::pin(sem.acquire(1));
        let mut w2 = Box::pin(sem.acquire(1));
        let mut w3 = Box::pin(sem.acquire(1));
        assert!(w1.as_mut().poll(cx).is_pending());
        assert!(w2.as_mut().poll(cx).is_pending());
        assert!(w3.as_mut().poll(cx).is_pending());

        // Releasing both permits wakes w1 and w2; w3 stays queued.
        drop(initial);

        // Cancelling w1 must hand its permit on to w3.
        drop(w1);

        assert!(w2.as_mut().poll(cx).is_ready());
        assert!(w3.as_mut().poll(cx).is_ready());
    }

    // Cancelling a parked (never-woken) waiter must leave the queue consistent so
    // a later release still reaches the next waiter.
    #[test]
    fn parked_acquire_cancelled_then_release() {
        let cx = &mut noop_cx();
        let sem = Semaphore::new(1);
        let held = sem.try_acquire(1).unwrap();

        let mut a = Box::pin(sem.acquire(1));
        let mut b = Box::pin(sem.acquire(1));
        assert!(a.as_mut().poll(cx).is_pending());
        assert!(b.as_mut().poll(cx).is_pending());

        // Cancel the parked front waiter, then release a permit.
        drop(a);
        drop(held);

        assert!(b.as_mut().poll(cx).is_ready());
    }
}
