//! [<img alt="github" src="https://img.shields.io/badge/github-udoprog/unsync?style=for-the-badge&logo=github" height="20">](https://github.com/udoprog/unsync)
//! [<img alt="crates.io" src="https://img.shields.io/crates/v/unsync.svg?style=for-the-badge&color=fc8d62&logo=rust" height="20">](https://crates.io/crates/unsync)
//! [<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-unsync?style=for-the-badge&logoColor=white&logo=data:image/svg+xml;base64,PHN2ZyByb2xlPSJpbWciIHhtbG5zPSJodHRwOi8vd3d3LnczLm9yZy8yMDAwL3N2ZyIgdmlld0JveD0iMCAwIDUxMiA1MTIiPjxwYXRoIGZpbGw9IiNmNWY1ZjUiIGQ9Ik00ODguNiAyNTAuMkwzOTIgMjE0VjEwNS41YzAtMTUtOS4zLTI4LjQtMjMuNC0zMy43bC0xMDAtMzcuNWMtOC4xLTMuMS0xNy4xLTMuMS0yNS4zIDBsLTEwMCAzNy41Yy0xNC4xIDUuMy0yMy40IDE4LjctMjMuNCAzMy43VjIxNGwtOTYuNiAzNi4yQzkuMyAyNTUuNSAwIDI2OC45IDAgMjgzLjlWMzk0YzAgMTMuNiA3LjcgMjYuMSAxOS45IDMyLjJsMTAwIDUwYzEwLjEgNS4xIDIyLjEgNS4xIDMyLjIgMGwxMDMuOS01MiAxMDMuOSA1MmMxMC4xIDUuMSAyMi4xIDUuMSAzMi4yIDBsMTAwLTUwYzEyLjItNi4xIDE5LjktMTguNiAxOS45LTMyLjJWMjgzLjljMC0xNS05LjMtMjguNC0yMy40LTMzLjd6TTM1OCAyMTQuOGwtODUgMzEuOXYtNjguMmw4NS0zN3Y3My4zek0xNTQgMTA0LjFsMTAyLTM4LjIgMTAyIDM4LjJ2LjZsLTEwMiA0MS40LTEwMi00MS40di0uNnptODQgMjkxLjFsLTg1IDQyLjV2LTc5LjFsODUtMzguOHY3NS40em0wLTExMmwtMTAyIDQxLjQtMTAyLTQxLjR2LS42bDEwMi0zOC4yIDEwMiAzOC4ydi42em0yNDAgMTEybC04NSA0Mi41di03OS4xbDg1LTM4Ljh2NzUuNHptMC0xMTJsLTEwMiA0MS40LTEwMi00MS40di0uNmwxMDItMzguMiAxMDIgMzguMnYuNnoiPjwvcGF0aD48L3N2Zz4K" height="20">](https://docs.rs/unsync)
//! [<img alt="build status" src="https://img.shields.io/github/workflow/status/udoprog/unsync/CI/main?style=for-the-badge" height="20">](https://github.com/udoprog/unsync/actions?query=branch%3Amain)
//!
//! Unsynchronized synchronization primitives for async Rust.
//!
//! This crate provides a fairly simple set of synchronization primitives which
//! are explicitly `!Send` and `!Sync`. This makes them useful for use in
//! singlethreaded systems like [yew].
//!
//! You can think of this as a modern replacement to the now-deprecated
//! [futures::unsync] module.
//!
//! <br>
//!
//! # Why do you want `!Send` / `!Sync` synchronization primitives?
//!
//! Having unsynchronized sync primitives might seem *weird* since they are
//! largely used for inter-task communication across threads.
//!
//! The need for such primitives increase as we are seeing more singlethreaded
//! systems like [yew] mature and are receiving richer support for async
//! programming. In order to make them as efficient as possible it's useful that
//! they are written with unsynchronized systems and constraints in mind so that
//! they don't have to make use of atomics and locks.
//!
//! The overhead of synchronization in real systems should be minor because of
//! the role channels play in typical applications and they are optimized for
//! uncontended use. But unsynchronized code still has the ability to optimize
//! better for both performance and size.
//!
//! In one of my applications replacing the use of `tokio::sync::oneshot` with
//! `unsync::oneshot` reduced the size of the resulting binary by 30kb (10kb
//! when optimized with `wasm-opt`). Synthetic benchmarks in this project hints
//! at the unsync channels being about twice as fast for optimized builds when
//! used inside of a [LocalSet].
//!
//! I haven't dug too deep into the specifics of why this is, but as long as
//! this is the case I'd like to have access to drop in replacements allowing
//! someone to tap into these benefits.
//!
//! [yew]: <https://yew.rs>
//! [LocalSet]: https://docs.rs/tokio/latest/tokio/task/struct.LocalSet.html
//!
//! <br>
//!
//! # Usage
//!
//! Add the following to your `Cargo.toml`:
//!
//! ```toml
//! unsync = "0.1.1"
//! ```
//!
//! <br>
//!
//! # Examples
//!
//! ```
//! use unsync::spsc::{channel, Sender, Receiver};
//! use std::error::Error;
//! use tokio::task;
//!
//! async fn receiver(mut rx: Receiver<u32>) -> Vec<u32> {
//!     let mut out = Vec::new();
//!
//!     while let Some(m) = rx.recv().await {
//!         out.push(m);
//!     }
//!
//!     out
//! }
//!
//! async fn sender(mut tx: Sender<u32>) -> Result<(), Box<dyn Error>> {
//!     for n in 0..1000 {
//!         tx.send(n).await?;
//!     }
//!
//!     Ok(())
//! }
//!
//! async fn run() -> Result<(), Box<dyn Error>> {
//!     let (tx, rx) = channel(4);
//!
//!     let _ = task::spawn_local(sender(tx));
//!     let out = task::spawn_local(receiver(rx)).await?;
//!
//!     let expected = (0..1000).collect::<Vec<u32>>();
//!     assert_eq!(out, expected);
//!     Ok(())
//! }
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> Result<(), Box<dyn Error>> {
//! # task::LocalSet::new().run_until(run()).await?;
//! # Ok(()) }
//! ```
//!
//! [futures::unsync]: <https://docs.rs/futures/0.1.31/futures/unsync/index.html>

#![deny(missing_docs)]
#![deny(rust_2018_idioms, unsafe_op_in_unsafe_fn)]
#![allow(clippy::result_unit_err)]

mod bi_rc;
mod broad_rc;
pub mod broadcast;
pub mod once_cell;
pub mod oneshot;
mod semaphore;
pub mod spsc;
pub mod wait_list;

#[doc(no_inline)]
pub use once_cell::OnceCell;
pub use semaphore::Semaphore;
pub use semaphore::SemaphorePermit;

#[cfg(test)]
mod test_util {
    use std::sync::Arc;
    use std::task;

    macro_rules! noop_cx {
        ($cx:ident) => {
            let waker = crate::test_util::noop_waker();
            let $cx = &mut std::task::Context::from_waker(&waker);
        };
    }
    pub(crate) use noop_cx;

    pub(crate) fn noop_waker() -> task::Waker {
        struct Noop;
        impl task::Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        task::Waker::from(Arc::new(Noop))
    }
}
