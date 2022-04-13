//! A set of fairly simple `!Send` and `!Sync` channels useful for communicating
//! between different parts in single-threaded systems like [yew].
//!
//! You can think of this as a modern replacement to the now-deprecated
//! [futures::unsync] module.
//!
//! # Why do you want `!Send` channels?
//!
//! Having `!Send` channels might seem *weird* since channels are largely used
//! for inter-thread (or inter-task) communication.
//!
//! As we are seeing more systems like [yew] mature which runs in the
//! singlethreaded browser environment and is receiving richer support for async
//! programming the need for such channels increases. And in order to make them
//! as efficient as possible it's useful that they are written with
//! singlethreaded systems in mind so that they don't have to bother with
//! atomics and locks.
//!
//! Now the overhead in real systems should be minor - the available uncontended
//! locks and atomics largely behave as their non-synchronized counterparts in
//! singlethreaded systems. But they introduce code which the compiler cannot as
//! easily reason about to in many cases simply optimize away.
//!
//! Below is a very simple web application I'm working on, and this is the size
//! difference when switching from Tokio to unsync:
//!
//! ```text
//! unoptimized tokio: 10307604
//! unoptimized unsync: 10272626
//!
//! optimized tokio: 1615064
//! optimized unsync: 1605474
//! ```
//!
//! So we save about 10kb of WASM with this simple change. 30kb in the
//! unoptimized build. Those are 10kb you won't have to ship to your customer.
//!
//! [optimize away]:
//! [futures::unsync]: https://docs.rs/futures/0.1.31/futures/unsync/index.html
//! [yew]: https://yew.rs

mod bi_rc;
mod broad_rc;
pub mod broadcast;
pub mod oneshot;
pub mod spsc;
