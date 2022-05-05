//! [`WaitList`] is an intrusively linked list of futures waiting on an event.
#![warn(unsafe_op_in_unsafe_fn)]

use std::cell::Cell;
use std::cell::UnsafeCell;
use std::fmt;
use std::fmt::Debug;
use std::future::Future;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::process;
use std::ptr::NonNull;
use std::task;
use std::task::Poll;

/// An intrusively linked list of futures waiting on an event.
///
/// This is the most fundamental primitive to many of the synchronization utilities provided by
/// this crate.
///
/// # Examples
///
/// A simple unfair, unsynchronized async mutex.
///
/// ```
/// use std::cell::Cell;
/// use std::cell::UnsafeCell;
/// use std::ops::Deref;
/// use std::ops::DerefMut;
///
/// use unsync::wait_list;
/// use unsync::wait_list::WaitList;
///
/// pub struct Mutex<T> {
///     data: UnsafeCell<T>,
///     locked: Cell<bool>,
///     waiters: WaitList<(), ()>,
/// }
///
/// impl<T> Mutex<T> {
///     pub const fn new(data: T) -> Self {
///         Self {
///             data: UnsafeCell::new(data),
///             locked: Cell::new(false),
///             waiters: WaitList::new(),
///         }
///     }
///
///     pub async fn lock(&self) -> MutexGuard<'_, T> {
///         while self.locked.replace(true) {
///             self.waiters.wait(()).await;
///         }
///         MutexGuard { mutex: self }
///     }
/// }
///
/// pub struct MutexGuard<'mutex, T> {
///     mutex: &'mutex Mutex<T>,
/// }
///
/// impl<T> Deref for MutexGuard<'_, T> {
///     type Target = T;
///
///     fn deref(&self) -> &Self::Target {
///         unsafe { &*self.mutex.data.get() }
///     }
/// }
///
/// impl<T> DerefMut for MutexGuard<'_, T> {
///     fn deref_mut(&mut self) -> &mut Self::Target {
///         unsafe { &mut *self.mutex.data.get() }
///     }
/// }
///
/// impl<T> Drop for MutexGuard<'_, T> {
///     fn drop(&mut self) {
///         self.mutex.locked.set(false);
///         self.mutex.waiters.borrow().wake_one(());
///     }
/// }
/// ```
pub struct WaitList<I, O> {
    /// Whether the wait list is currently borrowed.
    ///
    /// This flag asserts unique access to both `inner` and every `Waiter` in the list.
    borrowed: Cell<bool>,

    /// Inner state of the wait list, protected by the above boolean.
    inner: UnsafeCell<Inner<I, O>>,
}

struct Inner<I, O> {
    /// The head of the queue; the oldest waiter.
    ///
    /// If this is `None`, the list is empty.
    head: Option<NonNull<UnsafeCell<Waiter<I, O>>>>,

    /// The tail of the queue; the newest waiter.
    ///
    /// Whether this is `None` must remain in sync with whether `head` is `None`.
    tail: Option<NonNull<UnsafeCell<Waiter<I, O>>>>,
}

/// A waiter in the above list.
///
/// Each waiter in the list is wrapped in an `UnsafeCell` because there are several places that may
/// hold a reference two it (the linked list and the waiting future). The `UnsafeCell` is guarded
/// by the `WaitList::borrowed` boolean.
///
/// Each `Waiter` is stored by its waiting future, and will be automatically removed from the
/// linked list by `dequeue` when the future completes or is cancelled.
struct Waiter<I, O> {
    /// The next waiter in the linked list.
    next: Option<NonNull<UnsafeCell<Waiter<I, O>>>>,

    /// The previous waiter in the linked list.
    prev: Option<NonNull<UnsafeCell<Waiter<I, O>>>>,

    /// Extra state held by each waiter.
    state: State<I, O>,

    /// The waker associated with each task.
    ///
    /// `None` indicates that the waiter has been woken and dequeued.
    waker: Option<task::Waker>,
}

union State<I, O> {
    /// The waiter has not been woken; `Waiter::waker` is `Some`.
    input: ManuallyDrop<I>,

    /// The waiter has been woken; `Waiter::waker` is `None`.
    output: ManuallyDrop<O>,
}

impl<I, O> Drop for Waiter<I, O> {
    fn drop(&mut self) {
        if self.waker.is_some() {
            unsafe { ManuallyDrop::drop(&mut self.state.input) };
        } else {
            unsafe { ManuallyDrop::drop(&mut self.state.output) };
        }
    }
}

impl<I, O> WaitList<I, O> {
    /// Create a new empty `WaitList`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            borrowed: Cell::new(false),
            inner: UnsafeCell::new(Inner {
                head: None,
                tail: None,
            }),
        }
    }

    /// Attempt to borrow uniquely the contents of this list, returning [`None`] if it is already
    /// currently borrowed.
    #[must_use]
    pub fn try_borrow(&self) -> Option<Borrowed<'_, I, O>> {
        if self.borrowed.replace(true) {
            return None;
        }
        Some(Borrowed { list: self })
    }

    /// Borrow uniquely the contents of this list.
    ///
    /// # Panics
    ///
    /// Panics if the list is already borrowed.
    #[must_use]
    pub fn borrow(&self) -> Borrowed<'_, I, O> {
        self.try_borrow()
            .expect("attempted to borrow `WaitList` while it is already borrowed")
    }

    /// Wait on the wait list.
    ///
    /// The returned future resolves once [`Borrowed::wake_one`] is called and it is at the front
    /// of the queue.
    ///
    /// # Panics
    ///
    /// Panics if the list is currently borrowed.
    pub async fn wait(&self, input: I) -> O {
        let waiter = UnsafeCell::new(Waiter {
            // Both start out as `None` but are filled in later.
            next: None,
            prev: None,
            state: State {
                input: ManuallyDrop::new(input),
            },
            waker: Some(CloneWaker.await),
        });

        // SAFETY: `waiter` is a local variable so we have unique access to it.
        unsafe { WaitInner::new(self, &waiter) }.await;

        let mut waiter = ManuallyDrop::new(waiter.into_inner());
        debug_assert!(waiter.waker.is_none());
        unsafe { ManuallyDrop::take(&mut waiter.state.output) }
    }
}

impl<I, O> Debug for WaitList<I, O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("WaitList")
    }
}

/// The borrowed contents of a `WaitList`.
///
/// For a given `WaitList` only one of these may exist at once.
#[derive(Debug)]
pub struct Borrowed<'wait_list, I, O> {
    list: &'wait_list WaitList<I, O>,
}

impl<'wait_list, I, O> Borrowed<'wait_list, I, O> {
    fn inner(&self) -> &Inner<I, O> {
        // SAFETY: In order to create this type, the `WaitList` must be borrowed uniquely, so we
        // effectively have an `&mut Inner<T>`.
        unsafe { &*self.list.inner.get() }
    }
    fn inner_mut(&mut self) -> &mut Inner<I, O> {
        // SAFETY: As above.
        unsafe { &mut *self.list.inner.get() }
    }

    fn head(&self) -> Option<&UnsafeCell<Waiter<I, O>>> {
        // SAFETY: The head pointer of the linked list must always be valid.
        Some(unsafe { self.inner().head?.as_ref() })
    }

    /// Check whether there are any futures waiting in this list.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner().head.is_none()
    }

    /// Retrieve a shared reference to the input given by the head entry in the list, if there is
    /// one.
    #[must_use]
    pub fn head_input(&self) -> Option<&I> {
        // SAFETY: We have set `borrowed`, so we can access any entry in the list.
        Some(unsafe { &(*self.head()?.get()).state.input })
    }

    /// Retrieve a unique reference to the input given by the head entry in the list, if there is
    /// one.
    #[must_use]
    pub fn head_input_mut(&mut self) -> Option<&mut I> {
        // SAFETY: We have set `borrowed`, so we can access any entry in the list.
        Some(unsafe { &mut (*self.head()?.get()).state.input })
    }

    /// Add a waiter node to the end of this linked list.
    ///
    /// # Safety
    ///
    /// - `waiter` must be the only reference to that object.
    /// - `waiter` must be a valid pointer until it is removed.
    unsafe fn enqueue(&mut self, waiter: &UnsafeCell<Waiter<I, O>>) {
        // Set the previous waiter to the current tail of the queue, if there was one.
        unsafe { &mut *waiter.get() }.prev = self.inner_mut().tail;

        let waiter_ptr = NonNull::from(waiter);

        // Update the old tail's next pointer
        if let Some(prev) = self.inner_mut().tail {
            let prev = unsafe { &mut *prev.as_ref().get() };
            debug_assert_eq!(prev.next, None);
            prev.next = Some(waiter_ptr);
        }

        // Set the waiter as the new tail of the linked list
        self.inner_mut().tail = Some(waiter_ptr);

        // Also set it as the head if there isn't currently a head.
        self.inner_mut().head.get_or_insert(waiter_ptr);
    }

    /// Remove a waiter node from an arbitrary position in the linked list.
    ///
    /// # Safety
    ///
    /// - `waiter` must be a waiter in this queue.
    unsafe fn dequeue(&mut self, waiter: &UnsafeCell<Waiter<I, O>>) {
        let waiter_ptr = Some(NonNull::from(waiter));
        let waiter = unsafe { &mut *waiter.get() };

        let prev = waiter.prev;
        let next = waiter.next;

        // Update the pointer of the previous node, or the queue head
        let prev_next_pointer = match waiter.prev {
            Some(prev) => &mut unsafe { &mut *prev.as_ref().get() }.next,
            None => &mut self.inner_mut().head,
        };
        debug_assert_eq!(*prev_next_pointer, waiter_ptr);
        *prev_next_pointer = next;

        // Update the pointer of the next node, or the queue tail
        let next_prev_pointer = match waiter.next {
            Some(next) => &mut unsafe { &mut *next.as_ref().get() }.prev,
            None => &mut self.inner_mut().tail,
        };
        debug_assert_eq!(*next_prev_pointer, waiter_ptr);
        *next_prev_pointer = prev;
    }

    /// Wake and dequeue the first waiter in the queue, if there is one.
    pub fn wake_one(&mut self, output: O) {
        let head = match self.head() {
            Some(head) => head,
            None => return,
        };

        let head_waiter = unsafe { &mut *head.get() };

        // Take the `Waker`, both for later waking and to mark it as woken
        let waker = head_waiter.waker.take().unwrap();

        // Replace the old input with our output. It is important that this is dropped later and
        // not now so we don't have to deal with panics.
        let _input = unsafe { ManuallyDrop::take(&mut head_waiter.state.input) };
        head_waiter.state.output = ManuallyDrop::new(output);

        // Extend the lifetime of `head` so the `self` borrow below doesn't conflict with it.
        // SAFETY: The safety contract of `enqueue` ensures the waiter lives long enough.
        let head = unsafe { NonNull::from(head).as_ref() };

        // Dequeue the first waiter now that it's not necessary to keep it in the queue.
        unsafe { self.dequeue(head) };

        // Wake the waker last, to ensure that if this panics nothing goes wrong.
        waker.wake();
    }
}

impl<I, O> Drop for Borrowed<'_, I, O> {
    fn drop(&mut self) {
        debug_assert!(self.list.borrowed.get());
        self.list.borrowed.set(false);
    }
}

/// The inner future used by a waiting operation.
struct WaitInner<'list, 'waiter, I, O> {
    list: &'list WaitList<I, O>,
    waiter: &'waiter UnsafeCell<Waiter<I, O>>,
}

impl<'list, 'waiter, I, O> WaitInner<'list, 'waiter, I, O> {
    /// # Safety
    ///
    /// - `waiter` must be the only reference to that object.
    unsafe fn new(list: &'list WaitList<I, O>, waiter: &'waiter UnsafeCell<Waiter<I, O>>) -> Self {
        // SAFETY: Upheld by the caller.
        unsafe { list.borrow().enqueue(waiter) };
        Self { list, waiter }
    }
}

impl<I, O> Future for WaitInner<'_, '_, I, O> {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        let _guard = self.list.borrow();

        let old_waker = &mut unsafe { &mut *self.waiter.get() }.waker;
        match old_waker {
            // No need to update the waker
            Some(same_waker) if same_waker.will_wake(cx.waker()) => {}

            // Replace the old waker with the current one
            Some(_) => *old_waker = Some(cx.waker().clone()),

            // No waker means we have been dequeued
            None => return Poll::Ready(()),
        }
        Poll::Pending
    }
}

impl<I, O> Drop for WaitInner<'_, '_, I, O> {
    fn drop(&mut self) {
        let mut list = match self.list.try_borrow() {
            Some(guard) => guard,
            // Panicking isn't enough because that would allow the `waiter` to be used after it's
            // freed.
            None => process::abort(),
        };

        if unsafe { &*self.waiter.get() }.waker.is_some() {
            unsafe { list.dequeue(self.waiter) };
        }
    }
}

struct CloneWaker;
impl Future for CloneWaker {
    type Output = task::Waker;
    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Self::Output> {
        Poll::Ready(cx.waker().clone())
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::task;
    use std::task::Poll;

    use super::WaitList;

    #[test]
    fn wake_empty() {
        let list = <WaitList<(), ()>>::new();
        list.borrow().wake_one(());
        list.borrow().wake_one(());
        assert_eq!(list.borrow().head_input(), None);
    }

    #[test]
    fn cancel() {
        noop_cx!(cx);

        let list = <WaitList<Box<u32>, ()>>::new();
        let mut future = Box::pin(list.wait(Box::new(5)));
        for _ in 0..10 {
            assert_eq!(future.as_mut().poll(cx), Poll::Pending);
        }
        drop(future);
    }

    #[test]
    fn wake_single() {
        noop_cx!(cx);

        let list = <WaitList<Box<u32>, Box<u32>>>::new();
        let mut future = Box::pin(list.wait(Box::new(5)));
        assert_eq!(future.as_mut().poll(cx), Poll::Pending);
        assert_eq!(**list.borrow().head_input().unwrap(), 5);

        list.borrow().wake_one(Box::new(6));
        assert_eq!(future.as_mut().poll(cx), Poll::Ready(Box::new(6)));
        assert_eq!(list.borrow().head_input(), None);
    }

    #[test]
    fn wake_multiple() {
        noop_cx!(cx);
        let list = <WaitList<Box<u32>, Box<u32>>>::new();
        let mut f1 = Box::pin(list.wait(Box::new(1)));
        let mut f2 = Box::pin(list.wait(Box::new(2)));
        let mut f3 = Box::pin(list.wait(Box::new(3)));
        assert_eq!(f1.as_mut().poll(cx), Poll::Pending);
        assert_eq!(f2.as_mut().poll(cx), Poll::Pending);
        list.borrow().wake_one(Box::new(11));
        assert_eq!(f3.as_mut().poll(cx), Poll::Pending);
        list.borrow().wake_one(Box::new(12));
        list.borrow().wake_one(Box::new(99));
        list.borrow().wake_one(Box::new(99));
        assert_eq!(f2.as_mut().poll(cx), Poll::Ready(Box::new(12)));
        assert_eq!(f1.as_mut().poll(cx), Poll::Ready(Box::new(11)));
    }

    #[test]
    fn drop_in_middle() {
        noop_cx!(cx);

        let list = <WaitList<Box<u32>, ()>>::new();
        let mut f1 = Box::pin(list.wait(Box::new(1)));
        let mut f2 = Box::pin(list.wait(Box::new(2)));
        let mut f3 = Box::pin(list.wait(Box::new(3)));
        assert_eq!(f1.as_mut().poll(cx), Poll::Pending);
        assert_eq!(f2.as_mut().poll(cx), Poll::Pending);
        assert_eq!(f3.as_mut().poll(cx), Poll::Pending);
        drop(f2);
        drop(f3);
        drop(f1);
        assert!(list.borrow().is_empty());
    }

    macro_rules! noop_cx {
        ($cx:ident) => {
            let waker = noop_waker();
            let $cx = &mut task::Context::from_waker(&waker);
        };
    }
    use noop_cx;

    fn noop_waker() -> task::Waker {
        struct Noop;
        impl task::Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        task::Waker::from(Arc::new(Noop))
    }
}