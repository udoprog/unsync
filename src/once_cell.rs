//! [`OnceCell`] holds a value that can be written to only once.

use std::cell::UnsafeCell;
use std::convert::Infallible;
use std::fmt;
use std::fmt::Debug;
use std::future::Future;
use std::mem;

use crate::wait_list::WaitList;

/// A value that can be written to only once.
///
/// A `OnceCell` is typically used for global variables that need to be initialized once on first
/// use, but need no further changes. This `OnceCell` allows the initialization procedure to be
/// asynchronous.
pub struct OnceCell<T> {
    state: UnsafeCell<State<T>>,
    /// The list of tasks waiting for the initialization to complete.
    ///
    /// When it completes succesfully, this entire list will be woken. When it fails to
    /// complete, the first task on this list is woken and is expected to continue the
    /// initialization.
    waiters: WaitList<(), ()>,
}

enum State<T> {
    /// The value has not been filled yet
    Uninit,
    /// A task is currently calling `get_or_init` or `get_or_try_init`
    Initializing,
    /// The value is present
    Initialized { value: T },
}

impl<T> OnceCell<T> {
    /// Create a new `OnceCell` with no value inside.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: UnsafeCell::new(State::Uninit),
            waiters: WaitList::new(),
        }
    }

    /// Get a shared reference to the inner value, returning `None` if it has not been set yet or is
    /// in the process of being set.
    #[must_use]
    pub fn get(&self) -> Option<&T> {
        match unsafe { &*self.state.get() } {
            State::Initialized { value } => Some(value),
            _ => None,
        }
    }

    /// Get a unique reference to the inner value, returning `None` if it has not been set yet or
    /// is in the process of being set.
    #[must_use]
    pub fn get_mut(&mut self) -> Option<&mut T> {
        match self.state.get_mut() {
            State::Initialized { value } => Some(value),
            _ => None,
        }
    }

    /// Set the contents of the cell to `value` if it is unset.
    ///
    /// In contrast to [`Self::insert`], if the cell is currently being initialized this function
    /// will not wait and will instead return [`SetResult::Initializing`] immediately.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::future::Future;
    /// use std::task::Poll;
    ///
    /// use unsync::once_cell::{OnceCell, SetResult};
    /// # let cx = &mut unsync::utils::noop_cx();
    ///
    /// let cell = OnceCell::new();
    ///
    /// let mut initializer = Box::pin(cell.get_or_init(|| async {
    ///     tokio::task::yield_now().await;
    ///     5
    /// }));
    ///
    /// assert_eq!(initializer.as_mut().poll(cx), Poll::Pending);
    /// // Indicates that the cell is currently initializing.
    /// assert_eq!(cell.set(6), SetResult::Initializing(6));
    ///
    /// assert_eq!(initializer.as_mut().poll(cx), Poll::Ready(&5));
    /// // Indicates that the cell has been initialized.
    /// assert_eq!(cell.set(6), SetResult::Initialized(&5, 6));
    /// ```
    ///
    /// Example showcasing a failing insertion through
    /// [OnceCell::get_or_try_init] being superseeded by a call to
    /// [OnceCell::set].
    ///
    /// ```
    /// use unsync::once_cell::{OnceCell, SetResult};
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let cell = OnceCell::<i32>::new();
    ///
    /// assert_eq!(cell.get_or_try_init(|| async { Err("error") }).await, Err("error"));
    ///
    /// assert_eq!(cell.get(), None);
    /// assert_eq!(cell.set(5), SetResult::Ok(&5));
    /// # }
    /// ```
    pub fn set(&self, value: T) -> SetResult<'_, T> {
        match unsafe { &*self.state.get() } {
            State::Uninit => {
                unsafe { *self.state.get() = State::Initialized { value } };
                SetResult::Ok(self.get().unwrap())
            }
            State::Initializing => SetResult::Initializing(value),
            State::Initialized { value: reference } => SetResult::Initialized(reference, value),
        }
    }

    /// Set the contents of the cell to `value` if it is unset, or wait for it to be set.
    ///
    /// Returns a `Result` indicating if inserting the given value was successful.
    ///
    /// If the cell is currently being initialized, this function will wait for that to complete —
    /// if you do not wish to wait in that case, use [`Self::set`] instead.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::once_cell::OnceCell;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let cell = OnceCell::new();
    /// cell.insert(5).await.unwrap();
    /// assert_eq!(cell.get(), Some(&5));
    /// assert_eq!(cell.insert(6).await, Err((&5, 6)));
    /// # }
    /// ```
    ///
    /// Example showing a call to [OnceCell::insert] which supersedes a failing
    /// [OnceCell::get_or_try_init].
    ///
    /// ```
    /// use std::future::Future;
    /// use std::task::Poll;
    ///
    /// use unsync::once_cell::{OnceCell, SetResult};
    /// # let cx = &mut unsync::utils::noop_cx();
    ///
    /// let cell = OnceCell::<i32>::new();
    ///
    /// let mut failer = Box::pin(cell.get_or_try_init(|| async {
    ///     tokio::task::yield_now().await;
    ///     Err("error")
    /// }));
    ///
    /// let mut succeeder = Box::pin(cell.insert(10));
    ///
    /// assert_eq!(failer.as_mut().poll(cx), Poll::Pending);
    /// assert_eq!(succeeder.as_mut().poll(cx), Poll::Pending);
    ///
    /// assert_eq!(failer.as_mut().poll(cx), Poll::Ready(Err("error")));
    /// assert_eq!(cell.set(0), SetResult::Initializing(0));
    /// assert_eq!(succeeder.as_mut().poll(cx), Poll::Ready(Ok(&10)));
    /// assert_eq!(cell.get(), Some(&10));
    /// ```
    pub async fn insert(&self, value: T) -> Result<&T, (&T, T)> {
        let mut value = Some(value);
        let reference = self
            .get_or_init(|| async { unsafe { value.take().unwrap_unchecked() } })
            .await;
        match value {
            Some(value) => Err((reference, value)),
            None => Ok(reference),
        }
    }

    /// Get the contents of the cell, initializing it with `f` if it was empty.
    ///
    /// If either `f` or the future it returns panic, this is propagated and the cell will stay
    /// uninitialized.
    ///
    /// This function will deadlock if called recursively.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::once_cell::OnceCell;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let cell = OnceCell::new();
    ///
    /// let value = cell.get_or_init(|| async { 13 }).await;
    /// assert_eq!(value, &13);
    ///
    /// let value = cell.get_or_init(|| async { unreachable!() }).await;
    /// assert_eq!(value, &13);
    /// # }
    /// ```
    pub async fn get_or_init<Fut, F>(&self, f: F) -> &T
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        match self
            .get_or_try_init(|| async { Ok::<_, Infallible>(f().await) })
            .await
        {
            Ok(val) => val,
            Err(infallible) => match infallible {},
        }
    }

    /// Get the contents of the cell, attempting to initialize it with `f` it it was empty.
    ///
    /// If either `f` or the future it returns panic, this is propagated and the cell will stay
    /// uninitialized.
    ///
    /// This function will deadlock if called recursively.
    pub async fn get_or_try_init<E, Fut, F>(&self, f: F) -> Result<&T, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        match unsafe { &*self.state.get() } {
            State::Uninit => {
                unsafe { *self.state.get() = State::Initializing };
            }
            State::Initializing => {
                self.waiters.wait(()).await;
                match unsafe { &*self.state.get() } {
                    // Initializing only resets to Uninit when there are no waiters - but we
                    // were just waiting, so there must've been waiters.
                    State::Uninit => unreachable!(),
                    // The previous initializer failed to initialize the cell (it panicked or
                    // errored). The job has now been passed down to us.
                    State::Initializing => {}
                    State::Initialized { value } => return Ok(value),
                }
            }
            State::Initialized { value } => return Ok(value),
        }

        // This guard's Drop implementation is activated when `f` errors or panics and performs the
        // necessary cleanup of notifying others that we have failed.
        struct Guard<'once_cell, T>(&'once_cell OnceCell<T>);

        impl<T> Drop for Guard<'_, T> {
            fn drop(&mut self) {
                // We failed to initialize, so attempt to pass the job of initialization onto
                // the next waiter.
                if self.0.waiters.borrow().wake_one(()).is_err() {
                    // Having failed that, no-one is initializing the cell, so we must set its
                    // state back to `Uninit`.
                    unsafe { *self.0.state.get() = State::Uninit };
                }
            }
        }

        let guard = Guard(self);

        let value = f().await?;
        unsafe { *self.state.get() = State::Initialized { value } };

        // Disarm the guard
        mem::forget(guard);

        let mut waiters = self.waiters.borrow();
        while waiters.wake_one(()).is_ok() {}

        Ok(self.get().unwrap())
    }

    /// Take the value out of the `OnceCell`, leaving it in an uninitialized state.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::once_cell::OnceCell;
    ///
    /// let mut cell = OnceCell::new();
    /// assert_eq!(cell.take(), None);
    ///
    /// cell.set(10).unwrap();
    /// assert_eq!(cell.take(), Some(10));
    /// assert_eq!(cell.take(), None);
    /// ```
    pub fn take(&mut self) -> Option<T> {
        mem::take(self).into_inner()
    }

    /// Consume this `OnceCell` and return the inner value, if there was any.
    ///
    /// # Examples
    ///
    /// ```
    /// use unsync::once_cell::OnceCell;
    ///
    /// assert_eq!(<OnceCell<i32>>::new().into_inner(), None);
    /// assert_eq!(<OnceCell<i32>>::from(10).into_inner(), Some(10));
    /// ```
    pub fn into_inner(self) -> Option<T> {
        match self.state.into_inner() {
            State::Uninit | State::Initializing => None,
            State::Initialized { value } => Some(value),
        }
    }
}

impl<T: Debug> Debug for OnceCell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut s = f.debug_struct("OnceCell");
        if let Some(inner) = self.get() {
            s.field("value", inner);
        }
        s.finish()
    }
}

impl<T> Default for OnceCell<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> From<T> for OnceCell<T> {
    fn from(value: T) -> Self {
        Self {
            state: UnsafeCell::new(State::Initialized { value }),
            waiters: WaitList::new(),
        }
    }
}

impl<T: PartialEq> PartialEq for OnceCell<T> {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}
impl<T: Eq> Eq for OnceCell<T> {}

impl<T: Clone> Clone for OnceCell<T> {
    fn clone(&self) -> Self {
        self.get().cloned().map_or_else(Self::new, Self::from)
    }
}

/// The return value of [`OnceCell::set`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetResult<'once_cell, T> {
    /// The cell was succesfully initialized. Contains a reference to the new inner value.
    Ok(&'once_cell T),

    /// The cell was in the process of being initialized by another task, and so could not be set.
    /// Contains the value passed into [`OnceCell::set`].
    Initializing(T),

    /// The cell was already initialized. Contains both a reference to the initialized value and
    /// the value passed into `set`.
    Initialized(&'once_cell T, T),
}

impl<'once_cell, T> SetResult<'once_cell, T> {
    /// Get a [`Result`] over whether the given value was successfully inserted into the cell.
    pub fn ok(self) -> Result<&'once_cell T, T> {
        match self {
            Self::Ok(value) => Ok(value),
            Self::Initializing(value) | Self::Initialized(_, value) => Err(value),
        }
    }

    /// Retrieve a shared reference to the value inside the [`OnceCell`] if one was present.
    pub fn value(&self) -> Option<&'once_cell T> {
        match self {
            Self::Ok(value) | Self::Initialized(value, _) => Some(value),
            Self::Initializing(_) => None,
        }
    }

    /// Panic if setting the value failed (the `OnceCell` was being initialized or not empty).
    #[track_caller]
    pub fn unwrap(self) -> &'once_cell T {
        match self {
            Self::Ok(value) => value,
            Self::Initializing(..) => panic!("`OnceCell` was being initialized"),
            Self::Initialized(..) => panic!("`OnceCell` was already initialized"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::task::Poll;

    use super::OnceCell;
    use crate::utils::noop_cx;

    #[test]
    fn insert_when_initializing() {
        let cx = &mut noop_cx();

        let cell = OnceCell::new();

        let iters = 3;

        let mut initializer = Box::pin(cell.get_or_init(|| async {
            for _ in 0..iters {
                tokio::task::yield_now().await;
            }
            5
        }));

        let mut inserter = Box::pin(cell.insert(6));

        for _ in 0..iters {
            assert_eq!(initializer.as_mut().poll(cx), Poll::Pending);
            assert_eq!(inserter.as_mut().poll(cx), Poll::Pending);
        }

        assert_eq!(initializer.as_mut().poll(cx), Poll::Ready(&5));
        assert_eq!(inserter.as_mut().poll(cx), Poll::Ready(Err((&5, 6))));
    }
}
