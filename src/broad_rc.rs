//! A broad-cell, also known as "why the heck is this permitted?".
//!
//! This is a very simple reference-counted data structure who's purpose is
//! exactly two things:
//! * Ensure that the guarded value is not de-allocated until all live
//!   references of [BroadRc] and [BroadWeak] have been dropped.
//! * Indicates when either all strong or weak references have been dropped so
//!   the other ends knows. Allowing them to have an idea when the other end
//!   "disconnects".
//!
//! Now it's true that this API is roughly available through [Rc][std::rc::Rc],
//! but it would be more awkward to wrap to use correctly.

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

impl<T> Inner<T> {
    /// Try and deallocate interior value of refcount has reached zero.
    unsafe fn try_dealloc(ptr: *mut Self) {
        unsafe {
            if (*ptr).weak + (*ptr).strong == 0 {
                let _ = Box::from_raw(ptr);
            }
        }
    }
}

/// A simple `!Send` reference counted container which can be held at exactly
/// two places.
///
/// The wrapped reference can be always unsafely accessed with an indication of
/// whether both references are alive or not at the same time through
/// [BroadRc::load].
pub struct BroadRc<T> {
    inner: NonNull<Inner<T>>,
}

impl<T> BroadRc<T> {
    /// Construct a new reference-counted container.
    pub(crate) fn new(value: T) -> Self {
        let inner = NonNull::from(Box::leak(Box::new(Inner {
            value: UnsafeCell::new(value),
            strong: 1,
            weak: 0,
        })));

        Self { inner }
    }

    /// Construct a new weak reference.
    pub(crate) fn weak(&self) -> BroadWeak<T> {
        unsafe {
            let inner = &mut (*self.inner.as_ptr());
            inner.weak = crate::utils::checked_increment(inner.weak);
            BroadWeak { inner: self.inner }
        }
    }

    /// Get the interior value and indicates with a boolean if there are no weak
    /// references available.
    ///
    /// # Safety
    ///
    /// Caller must ensure that the reference returned by `get_mut_unchecked` is
    /// only used by one caller at the same time.
    pub unsafe fn get_mut_unchecked(&self) -> (&mut T, bool) {
        let inner = unsafe { &mut (*self.inner.as_ptr()) };
        (inner.value.get_mut(), inner.weak != 0)
    }
}

pub struct BroadWeak<T> {
    inner: NonNull<Inner<T>>,
}

impl<T> BroadWeak<T> {
    /// Get the interior value and indicates with a boolean if there are no
    /// strong references available.
    ///
    /// # Safety
    ///
    /// Caller must ensure that the reference returned by `get_mut_unchecked` is
    /// only used by one caller at the same time.
    pub unsafe fn get_mut_unchecked(&self) -> (&mut T, bool) {
        let inner = unsafe { &mut (*self.inner.as_ptr()) };
        (inner.value.get_mut(), inner.strong != 0)
    }
}

impl<T> Drop for BroadRc<T> {
    fn drop(&mut self) {
        unsafe {
            let inner = self.inner.as_ptr();
            (*inner).strong = (*inner).strong.wrapping_sub(1);
            Inner::try_dealloc(inner);
        }
    }
}

impl<T> Drop for BroadWeak<T> {
    fn drop(&mut self) {
        unsafe {
            let inner = self.inner.as_ptr();
            (*inner).weak = (*inner).weak.wrapping_sub(1);
            Inner::try_dealloc(inner);
        }
    }
}
