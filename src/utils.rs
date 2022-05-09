use std::sync::Arc;
use std::task;

/// Increment `value` assuming that an overflow is unlikely. Calls
/// [std::process::abort] on overflows.
pub(crate) fn checked_increment(value: usize) -> usize {
    if value == usize::MAX {
        std::process::abort()
    }

    value + 1
}

/// Declare a no-op context named `$cx` which can be used for testing.
#[macro_export]
macro_rules! noop_cx {
    ($cx:ident) => {
        let waker = $crate::utils::noop_waker();
        let $cx = &mut std::task::Context::from_waker(&waker);
    };
}

#[doc(hidden)]
pub use noop_cx;

#[doc(hidden)]
pub fn noop_waker() -> task::Waker {
    struct Noop;
    impl task::Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }
    task::Waker::from(Arc::new(Noop))
}
