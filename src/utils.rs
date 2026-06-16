use core::mem;
use core::ptr;
use core::task;

#[cfg(feature = "std")]
pub(crate) fn abort() -> ! {
    std::process::abort()
}

#[cfg(not(feature = "std"))]
pub(crate) fn abort() -> ! {
    loop {}
}

/// Increment `value` assuming that an overflow is unlikely. Calls [`abort`] on
/// overflows.
#[cfg(feature = "alloc")]
pub(crate) fn checked_increment(value: usize) -> usize {
    if value == usize::MAX {
        abort()
    }

    value + 1
}

/// Create a no-op context which can be used for testing.
pub fn noop_cx() -> task::Context<'static> {
    const VTABLE: task::RawWakerVTable = task::RawWakerVTable::new(
        // clone
        |_| RAW,
        // wake
        |_| {},
        // wake_by_ref
        |_| {},
        // drop
        |_| {},
    );

    const RAW: task::RawWaker = task::RawWaker::new(ptr::null(), &VTABLE);

    // `Waker::from_raw` is unfortunately not `const fn` so it can't be used
    // SAFETY: `Waker` is `#[repr(transparent)]` over `RawWaker`
    static WAKER: task::Waker = unsafe { mem::transmute::<task::RawWaker, task::Waker>(RAW) };

    task::Context::from_waker(&WAKER)
}
