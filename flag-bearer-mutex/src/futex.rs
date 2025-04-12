use core::sync::atomic::{
    AtomicU32,
    Ordering::{Acquire, Relaxed, Release},
};

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1; // locked, no other threads waiting
const CONTENDED: u32 = 2; // locked, and other threads waiting (contended)

pub struct RawMutex {
    state: AtomicU32,
}

/// Safety: This is the same mutex as implemented by std for linux, based on futex.
unsafe impl lock_api::RawMutex for RawMutex {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: RawMutex = RawMutex {
        state: AtomicU32::new(0),
    };

    type GuardMarker = lock_api::GuardNoSend;

    #[inline]
    fn lock(&self) {
        if self
            .state
            .compare_exchange_weak(UNLOCKED, LOCKED, Acquire, Relaxed)
            .is_err()
        {
            self.lock_contended();
        }
    }

    #[inline]
    fn try_lock(&self) -> bool {
        self.state
            .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            .is_ok()
    }

    #[inline]
    unsafe fn unlock(&self) {
        if self.state.swap(UNLOCKED, Release) == CONTENDED {
            // We only wake up one thread. When that thread locks the mutex, it
            // will mark the mutex as CONTENDED (see lock_contended above),
            // which makes sure that any other waiting threads will also be
            // woken up eventually.
            self.wake();
        }
    }

    #[inline]
    fn is_locked(&self) -> bool {
        self.state.load(Relaxed) != UNLOCKED
    }
}

impl RawMutex {
    #[cold]
    fn lock_contended(&self) {
        // Spin first to speed things up if the lock is released quickly.
        let mut state = self.spin();

        // If it's unlocked now, attempt to take the lock
        // without marking it as contended.
        if state == UNLOCKED {
            match self
                .state
                .compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed)
            {
                Ok(_) => return, // Locked!
                Err(s) => state = s,
            }
        }

        loop {
            // Put the lock in contended state.
            // We avoid an unnecessary write if it as already set to CONTENDED,
            // to be friendlier for the caches.
            if state != CONTENDED && self.state.swap(CONTENDED, Acquire) == UNLOCKED {
                // We changed it from UNLOCKED to CONTENDED, so we just successfully locked it.
                return;
            }

            // Wait for the futex to change state, assuming it is still CONTENDED.
            //
            // Safety: these are the correct syscall parameters on linux
            // <https://www.man7.org/linux/man-pages/man2/futex.2.html>
            //   long syscall(SYS_futex, uint32_t *uaddr, int futex_op, uint32_t val,
            //     const struct timespec *timeout, /* or: uint32_t val2 */
            //     uint32_t *uaddr2, uint32_t val3)
            //
            // std uses `WAIT_BITSET` for the timeout,
            // which we don't need but no point changing it here.
            //
            // > Note: for FUTEX_WAIT, timeout is interpreted as a relative
            // > value.  This differs from other futex operations, where
            // > timeout is interpreted as an absolute value.  To obtain the
            // > equivalent of FUTEX_WAIT with an absolute timeout, employ
            // > FUTEX_WAIT_BITSET with val3 specified as
            // > FUTEX_BITSET_MATCH_ANY.
            unsafe {
                libc::syscall(
                    libc::SYS_futex,
                    self.state.as_ptr(),
                    libc::FUTEX_WAIT_BITSET | libc::FUTEX_PRIVATE_FLAG,
                    CONTENDED,
                    core::ptr::null::<libc::timespec>(), // no timeout
                    core::ptr::null::<u32>(), // This argument is unused for FUTEX_WAIT_BITSET.
                    !0u32, // A full bitmask, to make it behave like a regular FUTEX_WAIT.
                );
            }

            // Spin again after waking up.
            state = self.spin();
        }
    }

    fn spin(&self) -> u32 {
        // std uses 100 spins here, but I think less spinning is typically better (and it still performs well).
        // TODO: test with higher thread counts to see if thread::yield_now() could help.
        let mut spin = 4;
        loop {
            // We only use `load` (and not `swap` or `compare_exchange`)
            // while spinning, to be easier on the caches.
            let state = self.state.load(Relaxed);

            // We stop spinning when the mutex is UNLOCKED,
            // but also when it's CONTENDED.
            if state != LOCKED || spin == 0 {
                return state;
            }

            core::hint::spin_loop();
            spin -= 1;
        }
    }

    #[cold]
    fn wake(&self) {
        // Safety: these are the correct syscall parameters on linux
        // <https://www.man7.org/linux/man-pages/man2/futex.2.html>
        //   long syscall(SYS_futex, uint32_t *uaddr, int futex_op, uint32_t val,
        //     const struct timespec *timeout, /* or: uint32_t val2 */
        //     uint32_t *uaddr2, uint32_t val3)
        // > The arguments timeout, uaddr2, and val3 are ignored.
        // > Most commonly, val is specified as either 1 (wake up a single waiter)
        // > or INT_MAX (wake up all waiters).
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                self.state.as_ptr(),
                libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG,
                1,
            );
        }
    }
}
