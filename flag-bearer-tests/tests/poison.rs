//! A panic in user code that touches the semaphore state poisons it. The state
//! may have been left half-updated, so poisoning persists until `clear_poison`
//! (like [`std::sync::Mutex`]). A poisoned semaphore stops handing out permits —
//! `try_acquire` reports it via `TryAcquireError::Poisoned`, the blocking
//! `acquire` panics — but it does not cascade panics through unrelated callers
//! (e.g. dropping permits / inspecting state stays fine).

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    task::{Context, Poll},
};

use flag_bearer::{Builder, SemaphoreState};
use futures_util::task::noop_waker_ref;

/// A counter whose `acquire` panics while `armed`.
#[derive(Debug)]
struct Bomb {
    permits: usize,
    armed: bool,
}

impl SemaphoreState for Bomb {
    type Params = usize;
    type Permit = usize;

    fn acquire(&mut self, params: usize) -> Result<usize, usize> {
        assert!(!self.armed, "boom: user acquire panicked");
        if params <= self.permits {
            self.permits -= params;
            Ok(params)
        } else {
            Err(params)
        }
    }

    fn release(&mut self, permit: usize) {
        self.permits += permit;
    }
}

/// The poisoned semaphore must surface poison rather than hand out permits, and
/// must not panic on unrelated, non-acquiring operations.
fn assert_poisoned(s: &flag_bearer::Semaphore<Bomb>) {
    assert!(s.is_poisoned());

    // `try_acquire` reports poison through its error type rather than panicking.
    assert!(
        matches!(
            s.try_acquire(1),
            Err(flag_bearer::TryAcquireError::Poisoned(1))
        ),
        "try_acquire returns Poisoned when poisoned",
    );

    // The blocking `acquire` path can't carry poison in its error, so it panics.
    let acq = catch_unwind(AssertUnwindSafe(|| {
        let mut cx = Context::from_waker(noop_waker_ref());
        let mut fut = Box::pin(s.acquire(1));
        let _ = fut.as_mut().poll(&mut cx);
    }));
    assert!(acq.is_err(), "blocking acquire panics when poisoned");

    // Inspecting the (possibly half-updated) state must not cascade a panic.
    let armed = catch_unwind(AssertUnwindSafe(|| s.with_state(|b| b.armed)));
    assert!(armed.is_ok(), "with_state must not panic when poisoned");

    // None of the above operations clear the poison (only `clear_poison` does).
    assert!(s.is_poisoned(), "poison is sticky");
}

#[test]
fn panic_in_check_acquire_poisons() {
    let s = Builder::fifo().with_state(Bomb {
        permits: 0,
        armed: false,
    });
    let mut cx = Context::from_waker(noop_waker_ref());

    // A waiter queues at the front, waiting for a permit.
    let mut a = Box::pin(s.acquire(1));
    assert!(a.as_mut().poll(&mut cx).is_pending());
    assert!(!s.is_poisoned());

    // Arm the bomb, then trigger `check()` via with_state: it takes the leader's
    // params and calls `acquire`, which panics.
    let r = catch_unwind(AssertUnwindSafe(|| {
        s.with_state(|b| {
            b.armed = true;
            b.permits = 1;
        });
    }));
    assert!(r.is_err(), "the user panic propagates to the caller");

    assert_poisoned(&s);

    // The poison survives the offending waiter being cancelled: the state may be
    // corrupt regardless of which waiter triggered the panic.
    drop(a);
    assert!(
        s.is_poisoned(),
        "poison is not cleared by cancelling the waiter"
    );
}

#[test]
fn panic_in_try_acquire_poisons() {
    // No waiter is queued here, so there is no `None` leader node to key off of —
    // the panic happens in `try_acquire`'s own `acquire` call. The poison flag is
    // the only thing that records it.
    let s = Builder::fifo().with_state(Bomb {
        permits: 1,
        armed: true,
    });
    assert!(!s.is_poisoned());

    let r = catch_unwind(AssertUnwindSafe(|| s.try_acquire(1)));
    assert!(r.is_err(), "the user panic propagates to the caller");

    assert_poisoned(&s);
}

#[test]
fn panic_in_with_state_closure_poisons() {
    // A panic in the `with_state` closure itself may also have left the state
    // half-updated.
    let s = Builder::fifo().with_state(Bomb {
        permits: 1,
        armed: false,
    });
    assert!(!s.is_poisoned());

    let r = catch_unwind(AssertUnwindSafe(|| {
        s.with_state(|b| {
            b.permits = 999;
            panic!("boom: with_state closure panicked");
        });
    }));
    assert!(r.is_err(), "the user panic propagates to the caller");

    assert_poisoned(&s);
}

#[test]
fn clear_poison_recovers() {
    let s = Builder::fifo().with_state(Bomb {
        permits: 0,
        armed: false,
    });

    let r = catch_unwind(AssertUnwindSafe(|| {
        s.with_state(|b| {
            b.armed = true;
            panic!("boom");
        });
    }));
    assert!(r.is_err());
    assert!(s.is_poisoned());

    // Repair the state, then clear the poison.
    s.with_state(|b| {
        b.armed = false;
        b.permits = 1;
    });
    s.clear_poison();
    assert!(!s.is_poisoned());

    // The semaphore hands out permits again.
    assert!(s.try_acquire(1).is_ok());
}

#[test]
fn clear_poison_skips_dead_waiter() {
    let s = Builder::fifo().with_state(Bomb {
        permits: 0,
        armed: false,
    });
    let mut cx = Context::from_waker(noop_waker_ref());

    // A waiter whose `acquire` will panic during `check()`, and a live waiter
    // queued behind it.
    let mut dead = Box::pin(s.acquire(1));
    assert!(dead.as_mut().poll(&mut cx).is_pending());
    let mut live = Box::pin(s.acquire(1));
    assert!(live.as_mut().poll(&mut cx).is_pending());

    // Poison via `check()`: the panic takes `dead`'s params (before touching
    // `permits`, so `permits` ends up at 2).
    let r = catch_unwind(AssertUnwindSafe(|| {
        s.with_state(|b| {
            b.armed = true;
            b.permits = 2;
        });
    }));
    assert!(r.is_err());
    assert!(s.is_poisoned());

    // Repair and clear, then poke `check()`.
    s.with_state(|b| b.armed = false);
    s.clear_poison();
    s.with_state(|_| {});

    // The dead leader is skipped, so the live waiter behind it is served.
    assert!(
        matches!(live.as_mut().poll(&mut cx), Poll::Ready(_)),
        "live waiter behind the dead leader is served after clear_poison",
    );

    // `dead` can never complete; dropping it removes its node.
    drop(dead);
}
