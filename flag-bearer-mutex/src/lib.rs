//! A [`lock_api::RawMutex`] that is tuned for good performance for expected `flag_bearer` semaphore use cases.

#![warn(
    unsafe_op_in_unsafe_fn,
    clippy::missing_safety_doc,
    clippy::multiple_unsafe_ops_per_block,
    clippy::undocumented_unsafe_blocks
)]

cfg_if::cfg_if! {
    if #[cfg(target_os = "linux")] {
        // linux seems to have better perf with just futex than with parking_lot.
        #[path = "futex.rs"]
        mod default_raw;
    } else {
        #[path = "parking_lot.rs"]
        mod default_raw;
    }
}

pub use lock_api;

/// A [`lock_api::RawMutex`] that is tuned for good performance for expected `flag_bearer` semaphore use cases.
///
/// # Implementation details
/// * On linux, this uses a futex
/// * On all other platforms, this uses parking-lot.
pub struct RawMutex(default_raw::RawMutex);

/// Safety: This forwards all mutual exclusion responsilibity to the inner type.
unsafe impl lock_api::RawMutex for RawMutex {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: RawMutex = RawMutex(default_raw::RawMutex::INIT);

    type GuardMarker = lock_api::GuardNoSend;

    #[inline]
    fn lock(&self) {
        self.0.lock();
    }

    #[inline]
    fn try_lock(&self) -> bool {
        self.0.try_lock()
    }

    #[inline]
    unsafe fn unlock(&self) {
        // Safety: from caller
        unsafe { self.0.unlock() };
    }

    #[inline]
    fn is_locked(&self) -> bool {
        self.0.is_locked()
    }
}
