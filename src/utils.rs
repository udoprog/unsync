use std::mem;
use std::ptr;
use std::task;

/// Increment `value` assuming that an overflow is unlikely. Calls
/// [std::process::abort] on overflows.
pub(crate) fn checked_increment(value: usize) -> usize {
    if value == usize::MAX {
        std::process::abort()
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
