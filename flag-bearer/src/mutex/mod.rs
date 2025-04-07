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

/// A [`lock_api::RawMutex`] that is tuned for good performance for expected [`Semaphore`](crate::Semaphore) use cases.
///
/// # Implementation details
/// * On linux, this uses a futex
/// * On all other platforms, this uses parking-lot.
pub struct DefaultRawMutex(default_raw::RawMutex);

/// Safety: This forwards all mutual exclusion responsilibity to the inner type.
unsafe impl lock_api::RawMutex for DefaultRawMutex {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: DefaultRawMutex = DefaultRawMutex(default_raw::RawMutex::INIT);

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

cfg_if::cfg_if! {
    if #[cfg(all(test, loom))] {
        mod loom;
        pub use loom::Mutex;
    } else {
        pub use lock_api::Mutex;
    }
}
