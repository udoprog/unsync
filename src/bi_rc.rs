//! A bi-cell, also known as "why the heck is this permitted?".
//!
//! This is a very simple reference-counted data structure who's purpose is
//! exactly two things:
//! * Ensure that the guarded value is not de-allocated until all live
//!   references of [BiRc] have been dropped.
//! * Be able to flag when any of the two references of [BiRc] have been
//!   dropped.
//!
//! Now it's true that this API is roughly available through [Rc][std::rc::Rc],
//! but it would be more awkward to wrap to use correctly.

use std::cell::UnsafeCell;
use std::ptr::NonNull;

struct Inner<T> {
    /// The interior value being reference counted.
    value: UnsafeCell<T>,
    /// How many users we currently have.
    count: usize,
}

/// A simple `!Send` reference counted container which can be held at exactly
/// two places.
///
/// The wrapped reference can be always unsafely accessed with an indication of
/// whether both references are alive or not at the same time through
/// [BiRc::get_mut_unchecked].
pub struct BiRc<T> {
    inner: NonNull<Inner<T>>,
}

impl<T> BiRc<T> {
    /// Construct a new reference counted container.
    pub(crate) fn new(value: T) -> (Self, Self) {
        let inner = NonNull::from(Box::leak(Box::new(Inner {
            value: UnsafeCell::new(value),
            count: 2,
        })));

        (Self { inner }, Self { inner })
    }

    /// Get the interior value and indicates with a boolean if both ends of this
    /// value are alive.
    ///
    /// # Safety
    ///
    /// Caller must ensure that the reference returned by `get_mut_unchecked` is
    /// only used by one caller at the same time.
    pub unsafe fn get_mut_unchecked(&self) -> (&mut T, bool) {
        let inner = unsafe { &mut (*self.inner.as_ptr()) };
        (inner.value.get_mut(), inner.count == 2)
    }
}

impl<T> Drop for BiRc<T> {
    fn drop(&mut self) {
        unsafe {
            let inner = self.inner.as_ptr();
            let count = (*inner).count.wrapping_sub(1);
            (*inner).count = count;

            // De-allocate interior structure.
            if count == 0 {
                let _ = Box::from_raw(inner);
            }
        }
    }
}
