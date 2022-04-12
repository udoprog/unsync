//! Simple `!Send` and `!Sync` channels useful for communicating between
//! different parts in single-threaded systems like [yew].
//!
//! [yew]: https://yew.rs

mod bi_ref;
pub mod oneshot;
pub mod spsc;
