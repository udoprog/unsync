use std::cell::Cell;
use std::mem::ManuallyDrop;

use crate::wait_list::WaitList;

/// An asynchronous semaphore for permit acquisition.
///
/// A semaphore allows limiting access to a shared resource to a certain number of callers at a
/// time. It is created with a certain number of _permits_ which can be shared among tasks, and
/// tasks can wait for permits to become available. Semaphores are commonly used for rate limiting.
///
/// This semaphore supports both fair and unfair operations. There are two aspects of fairness to
/// consider:
/// 1. Whether a task that wants fewer permits can obtain those permits while a task that wants more
///     permits waits. The default [`acquire`] method does not allow this, but [`acquire_unfair`]
///     does.
/// 2. Whether a task can steal the permits from another task if that second task has not been
///    scheduled yet (so the permits have been released but have yet to be acquired). This kind of
///    unfairness improves throughput for tasks that rapidly release and acquire permits without
///    yielding, so by default it is used. However if permits are released with [`release_fair`],
///    they will be directly and fairly handed off to the first waiter in line, disallowing any
///    [`acquire`] call from opportunistically taking them.
///
/// In comparison to [Tokio's semaphore], this semaphore:
/// - Is `!Sync` (obviously).
/// - Does not support closing.
/// - Tracks the total number of permits as well as the current number of available permits.
/// - Does not place a limit on the total number of permits — you can go up to `usize::MAX`.
/// - Consistently uses `usize` to count permit numbers instead of using `u32` sometimes.
/// - Gives more control over the fairness algorithms used.
///
/// [`acquire`]: Self::acquire
/// [`acquire_unfair`]: Self::acquire_unfair
/// [`release_fair`]: SemaphorePermit::release_fair
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
    #[must_use]
    pub fn available_permits(&self) -> usize {
        self.permits.get()
    }

    /// Retrieve the total number of permits, including those currently handed out.
    #[must_use]
    pub fn total_permits(&self) -> usize {
        self.total_permits.get()
    }

    /// Add new permits to the semaphore.
    ///
    /// # Panics
    ///
    /// This function will panic if it would result in more than `usize::MAX` total permits.
    pub fn add_permits(&self, new_permits: usize) {
        self.total_permits
            .set(self.total_permits.get().checked_add(new_permits).unwrap());
        self.release_permits(new_permits, WakeUp::Unfair);
    }

    /// Add new permits to the semaphore, using a fair wakeup algorithm to ensure that the new
    /// permits won't be taken by any waiter other than the one at the front of the queue.
    ///
    /// # Panics
    ///
    /// This function will panic if it would result in more than `usize::MAX` total permits.
    pub fn add_permits_fair(&self, new_permits: usize) {
        self.total_permits
            .set(self.total_permits.get().checked_add(new_permits).unwrap());
        self.release_permits(new_permits, WakeUp::Fair);
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
            waiters.wake_one(fairness);
        }
    }

    /// Attempt to acquire permits from the semaphore immediately.
    ///
    /// [`None`] is returned if there are not enough permits available **or** a task is currently
    /// waiting for a permit.
    pub fn try_acquire(&self, to_acquire: usize) -> Option<SemaphorePermit<'_>> {
        // If a task is already waiting for some permits, we mustn't steal it.
        if !self.waiters.borrow().is_empty() {
            return None;
        }

        self.try_acquire_unfair(to_acquire)
    }

    /// Attempt to acquire permits from the semaphore immediately, potentially unfairly stealing
    /// permits from a task that is waiting for permits.
    ///
    /// [`None`] is returned if there are not enough permits available, but **not** if a task is
    /// currently waiting for a permit.
    pub fn try_acquire_unfair(&self, to_acquire: usize) -> Option<SemaphorePermit<'_>> {
        if let Some(new_permits) = self.permits.get().checked_sub(to_acquire) {
            self.permits.set(new_permits);
            Some(SemaphorePermit {
                semaphore: self,
                permits: to_acquire,
            })
        } else {
            None
        }
    }

    /// Acquire permits from the semaphore.
    pub async fn acquire(&self, to_acquire: usize) -> SemaphorePermit<'_> {
        loop {
            if let Some(guard) = self.try_acquire(to_acquire) {
                break guard;
            }

            match self.waiters.wait(to_acquire).await {
                WakeUp::Unfair => continue,
                WakeUp::Fair => {
                    return SemaphorePermit {
                        semaphore: self,
                        permits: to_acquire,
                    };
                }
            }
        }
    }

    /// Acquire permits from the semaphore, potentially unfairly stealing permits from a task that
    /// is waiting for permits.
    pub async fn acquire_unfair(&self, to_acquire: usize) -> SemaphorePermit<'_> {
        loop {
            if let Some(guard) = self.try_acquire_unfair(to_acquire) {
                break guard;
            }

            match self.waiters.wait(to_acquire).await {
                WakeUp::Unfair => continue,
                WakeUp::Fair => {
                    return SemaphorePermit {
                        semaphore: self,
                        permits: to_acquire,
                    };
                }
            }
        }
    }
}

/// A RAII guard holding a number of permits obtained from a [`Semaphore`].
#[derive(Debug)]
pub struct SemaphorePermit<'semaphore> {
    semaphore: &'semaphore Semaphore,
    permits: usize,
}

impl<'semaphore> SemaphorePermit<'semaphore> {
    /// Retrieve a shared reference to the semaphore this guard is for.
    #[must_use]
    pub fn semaphore(&self) -> &'semaphore Semaphore {
        self.semaphore
    }

    /// Get the number of permits this guard holds.
    #[must_use]
    pub fn permits(&self) -> usize {
        self.permits
    }

    /// Leak the permits without releasing it to the semaphore.
    ///
    /// This reduces the total number of permits in the semaphore.
    pub fn leak(self) {
        let this = ManuallyDrop::new(self);
        let reduced_permits = this.semaphore.total_permits.get() - this.permits;
        this.semaphore.total_permits.set(reduced_permits);
    }

    /// Release the permits using a fair wakeup algorithm to ensure that the new permits won't be
    /// taken by any waiter other than the one at the front of the queue.
    ///
    /// To release the permits with an unfair wakeup algorithm, simply call the [`drop`] on this
    /// value or have it fall out of scope.
    pub fn release_fair(self) {
        let this = ManuallyDrop::new(self);
        this.semaphore.release_permits(this.permits, WakeUp::Fair);
    }
}

impl Drop for SemaphorePermit<'_> {
    fn drop(&mut self) {
        self.semaphore()
            .release_permits(self.permits(), WakeUp::Unfair);
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;

    use crate::noop_cx;
    use crate::Semaphore;

    #[test]
    fn permit_counting() {
        let semaphore = Semaphore::new(100);
        assert_eq!(semaphore.total_permits(), 100);
        assert_eq!(semaphore.available_permits(), 100);

        semaphore.add_permits(10);
        assert_eq!(semaphore.total_permits(), 110);
        assert_eq!(semaphore.available_permits(), 110);

        let guard = semaphore.try_acquire(20).unwrap();
        assert_eq!(semaphore.total_permits(), 110);
        assert_eq!(semaphore.available_permits(), 90);
        assert_eq!(guard.permits(), 20);

        drop(guard);
        assert_eq!(semaphore.total_permits(), 110);
        assert_eq!(semaphore.available_permits(), 110);

        semaphore.try_acquire(20).unwrap().leak();
        assert_eq!(semaphore.total_permits(), 90);
        assert_eq!(semaphore.available_permits(), 90);
    }

    #[test]
    fn skip_the_queue() {
        noop_cx!(cx);
        let semaphore = Semaphore::new(1);

        let mut future = Box::pin(semaphore.acquire(2));
        assert!(future.as_mut().poll(cx).is_pending());

        assert!(semaphore.try_acquire(1).is_none());
        semaphore.try_acquire_unfair(1).unwrap();

        drop(future);

        semaphore.try_acquire(1).unwrap();
    }

    #[test]
    fn unfairness() {
        noop_cx!(cx);
        let semaphore = Semaphore::new(1);

        let initial = semaphore.try_acquire(1).unwrap();

        let mut f1 = Box::pin(semaphore.acquire(1));
        assert!(f1.as_mut().poll(cx).is_pending());

        drop(initial);

        let mut f2 = Box::pin(semaphore.acquire(1));
        assert!(f2.as_mut().poll(cx).is_ready());
        assert!(f1.as_mut().poll(cx).is_ready());
    }

    #[test]
    fn fairness() {
        noop_cx!(cx);
        let semaphore = Semaphore::new(1);

        let initial = semaphore.try_acquire(1).unwrap();

        let mut f1 = Box::pin(semaphore.acquire(1));
        assert!(f1.as_mut().poll(cx).is_pending());

        initial.release_fair();

        let mut f2 = Box::pin(semaphore.acquire(1));
        assert!(f2.as_mut().poll(cx).is_pending());
        assert!(f1.as_mut().poll(cx).is_ready());
        assert!(f2.as_mut().poll(cx).is_ready());
    }
}
