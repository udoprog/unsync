//! A broad-cell, also known as "why the heck is this permitted?".
//!
//! This is a very simple reference-counted data structure who's purpose is
//! exactly two things:
//! * Ensure that the guarded value is not de-allocated until all live
//!   references of [BroadRef] have been dropped.
//! * Indicates when either all strong or weak references have been dropped.

use std::cell::UnsafeCell;
use std::ptr::NonNull;

struct Inner<T> {
    /// The interior value being reference counted.
    value: UnsafeCell<T>,
    /// How many users we currently have.
    strong: usize,
    /// How many weak reference we currently have.
    weak: usize,
}

/// A simple `!Send` reference counted container which can be held at exactly
/// two places.
///
/// The wrapped reference can be always unsafely accessed with an indication of
/// whether both references are alive or not at the same time through
/// [BroadRef::load].
pub struct BroadRef<T> {
    inner: NonNull<Inner<T>>,
}

impl<T> BroadRef<T> {
    /// Construct a new BroadRefed container.
    pub fn new(value: T) -> Self {
        let inner = NonNull::from(Box::leak(Box::new(Inner {
            value: UnsafeCell::new(value),
            strong: 1,
            weak: 0,
        })));

        Self { inner }
    }

    /// Construct a new weak reference.
    pub fn weak(&self) -> Weak<T> {
        unsafe {
            let mut inner = &mut (*self.inner.as_ptr());
            inner.weak += 1;

            Weak { inner: self.inner }
        }
    }

    /// Get the interior value and indicates with a boolean if there are no weak
    /// references available.
    ///
    /// # Safety
    ///
    /// Caller must ensure that the reference returned by `load` is only used by
    /// one caller at a time.
    pub unsafe fn load(&self) -> (&mut T, bool) {
        let inner = &mut (*self.inner.as_ptr());
        (inner.value.get_mut(), inner.weak != 0)
    }
}

pub struct Weak<T> {
    inner: NonNull<Inner<T>>,
}

impl<T> Weak<T> {
    /// Get the interior value and indicates with a boolean if there are no
    /// strong references available.
    ///
    /// # Safety
    ///
    /// Caller must ensure that the reference returned by `load` is only used by
    /// one caller at a time.
    pub unsafe fn load(&self) -> (&mut T, bool) {
        let inner = &mut (*self.inner.as_ptr());
        (inner.value.get_mut(), inner.strong != 0)
    }
}

impl<T> Drop for BroadRef<T> {
    fn drop(&mut self) {
        unsafe {
            let inner = self.inner.as_ptr();
            let count = (*inner).strong.wrapping_sub(1);
            (*inner).strong = count;

            if (*inner).weak + count == 0 {
                let _ = Box::from_raw(inner);
            }
        }
    }
}

impl<T> Drop for Weak<T> {
    fn drop(&mut self) {
        unsafe {
            let inner = self.inner.as_ptr();
            let count = (*inner).weak.wrapping_sub(1);
            (*inner).weak = count;

            if (*inner).strong + count == 0 {
                let _ = Box::from_raw(inner);
            }
        }
    }
}
