//! A crate for generic semaphore performing asynchronous permit acquisition.
//!
//! A semaphore maintains a set of permits. Permits are used to synchronize
//! access to a shared resource. A semaphore differs from a mutex in that it
//! can allow more than one concurrent caller to access the shared resource at a
//! time.
//!
//! When `acquire` is called and the semaphore has remaining permits, the
//! function immediately returns a permit. However, if no remaining permits are
//! available, `acquire` (asynchronously) waits until an outstanding permit is
//! dropped. At this point, the freed permit is assigned to the caller.
//!
//! This `Semaphore` is fair, and supports both FIFO and LIFO modes.
//! * In FIFO mode, this fairness means that permits are given out in the order
//!   they were requested.
//! * In LIFO mode, this fairness means that permits are given out in the reverse
//!   order they were requested.
//!
//! This fairness is also applied when `acquire` with high 'parameters' gets
//! involved, so if a call to `acquire` at the end of the queue requests
//! more permits than currently available, this can prevent another call to `acquire`
//! from completing, even if the semaphore has enough permits to complete it.
//!
//! This semaphore is generic, which means you can customise the state.
//! Examples:
//! * Using two counters, you can immediately remove permits,
//!   while there are some still in flight. This might be useful
//!   if you want to remove concurrency if failures are detected.
//! * There might be multiple quantities you want to limit over.
//!   Stacking multiple semaphores can be awkward and risk deadlocks.
//!   Instead, making the state contain all those quantities combined
//!   can simplify the queueing.

#![no_std]

use core::{hint::unreachable_unchecked, mem::ManuallyDrop, task::Waker};

#[cfg(test)]
extern crate std;

use parking_lot::Mutex;
use pin_list::PinList;

mod acquire;
pub use acquire::TryAcquireError;

/// The trait defining how [`Semaphore`]s behave.
pub trait SemaphoreState {
    /// What type is used to request permits.
    ///
    /// An example of this could be `usize` for a counting semaphore,
    /// if you want to support `acquire_many` type requests.
    type Params;

    /// The type representing the current permit allocation.
    ///
    /// If you have a counting semaphore, this could be the number
    /// of permits acquired. If this is more like a connection pool,
    /// this could be a specific object allocation.
    type Permit;

    /// Acquire a permit given the params.
    ///
    /// If a permit could not be acquired with the params, return an error with the
    /// original params back.
    fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params>;

    /// Return the permit back to the semaphore.
    ///
    /// Note: This is not guaranteed to be called for every acquire call.
    /// Permits can be modified or forgotten.
    fn release(&mut self, permit: Self::Permit);
}

/// Generic semaphore performing asynchronous permit acquisition.
///
/// A semaphore maintains a set of permits. Permits are used to synchronize
/// access to a shared resource. A semaphore differs from a mutex in that it
/// can allow more than one concurrent caller to access the shared resource at a
/// time.
///
/// When `acquire` is called and the semaphore has remaining permits, the
/// function immediately returns a permit. However, if no remaining permits are
/// available, `acquire` (asynchronously) waits until an outstanding permit is
/// dropped. At this point, the freed permit is assigned to the caller.
///
/// This `Semaphore` is fair, and supports both FIFO and LIFO modes.
/// * In FIFO mode, this fairness means that permits are given out in the order
///   they were requested.
/// * In LIFO mode, this fairness means that permits are given out in the reverse
///   order they were requested.
///
/// This fairness is also applied when `acquire` with high 'parameters' gets
/// involved, so if a call to `acquire` at the end of the queue requests
/// more permits than currently available, this can prevent another call to `acquire`
/// from completing, even if the semaphore has enough permits to complete it.
///
/// This semaphore is generic, which means you can customise the state.
/// Examples:
/// * Using two counters, you can immediately remove permits,
///   while there are some still in flight. This might be useful
///   if you want to remove concurrency if failures are detected.
/// * There might be multiple quantities you want to limit over.
///   Stacking multiple semaphores can be awkward and risk deadlocks.
///   Instead, making the state contain all those quantities combined
///   can simplify the queueing.
pub struct Semaphore<S: SemaphoreState> {
    state: Mutex<QueueState<S>>,
    fifo: bool,
}

impl<S: SemaphoreState + core::fmt::Debug> core::fmt::Debug for Semaphore<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_struct("Semaphore");
        d.field("fifo", &self.fifo);
        match self.state.try_lock() {
            Some(guard) => {
                d.field("state", &guard.state);
            }
            None => {
                d.field("state", &format_args!("<locked>"));
            }
        }
        d.finish_non_exhaustive()
    }
}

impl<S: SemaphoreState> Semaphore<S> {
    /// Create a new first-in-first-out semaphore with the given initial state.
    pub fn new_fifo(state: S) -> Self {
        Self::new_inner(state, true)
    }

    /// Create a new last-in-first-out semaphore with the given initial state.
    pub fn new_lifo(state: S) -> Self {
        Self::new_inner(state, false)
    }

    fn new_inner(state: S, fifo: bool) -> Self {
        let state = QueueState {
            state,
            // Safety: during acquire, we ensure that nodes in this queue
            // will never attempt to use a different queue to read the nodes.
            queue: PinList::new(unsafe { pin_list::id::DebugChecked::new() }),
        };
        Self {
            state: Mutex::new(state),
            fifo,
        }
    }

    /// Access the state with mutable access.
    ///
    /// This gives direct access to the state, be careful not to
    /// break any of your own state invariants. You can use this
    /// to peek at the current state, or to modify it, eg to add or
    /// remove permits from the semaphore.
    pub fn with_state<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let mut state = self.state.lock();
        let res = f(&mut state.state);
        state.check();
        res
    }
}

struct QueueState<S: SemaphoreState> {
    queue: PinList<PinQueue<S>>,
    state: S,
}

type PinQueue<S> = dyn pin_list::Types<
        Id = pin_list::id::DebugChecked,
        // Some(params), waker -> Pending
        // None, waker -> Invalid state.
        Protected = (Option<<S as SemaphoreState>::Params>, Waker),
        // Ok(permit) -> Ready
        // Err(params) -> Closed
        Removed = Result<<S as SemaphoreState>::Permit, <S as SemaphoreState>::Params>,
        Unprotected = (),
    >;

impl<S: SemaphoreState> QueueState<S> {
    #[inline]
    fn check(&mut self) {
        let mut leader = self.queue.cursor_front_mut();
        while let Some(p) = leader.protected_mut() {
            let params =
                p.0.take()
                    .expect("params should be in place. possibly the acquire method panicked");
            match self.state.acquire(params) {
                Ok(permit) => match leader.remove_current(Ok(permit)) {
                    Ok((_, waker)) => waker.wake(),
                    // Safety: with protected_mut, we have just made sure it is in the list
                    Err(_) => unsafe { unreachable_unchecked() },
                },
                Err(params) => {
                    p.0 = Some(params);
                    break;
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct Permit<'a, S: SemaphoreState> {
    sem: &'a Semaphore<S>,
    // this is never dropped because it's returned to the semaphore on drop
    permit: ManuallyDrop<S::Permit>,
}

impl<'a, S: SemaphoreState> Permit<'a, S> {
    /// Construct a new permit out of thin air, no waiting is required.
    pub fn out_of_thin_air(sem: &'a Semaphore<S>, permit: S::Permit) -> Self {
        Self {
            sem,
            permit: ManuallyDrop::new(permit),
        }
    }

    pub fn permit(&self) -> &S::Permit {
        &self.permit
    }

    pub fn permit_mut(&mut self) -> &mut S::Permit {
        &mut self.permit
    }

    pub fn semaphore(&self) -> &'a Semaphore<S> {
        self.sem
    }

    pub fn take(self) -> S::Permit {
        let mut this = ManuallyDrop::new(self);
        unsafe { ManuallyDrop::take(&mut this.permit) }
    }
}

impl<S: SemaphoreState> Drop for Permit<'_, S> {
    fn drop(&mut self) {
        // Safety: only taken on drop.
        let permit = unsafe { ManuallyDrop::take(&mut self.permit) };
        self.sem.with_state(|s| s.release(permit));
    }
}

#[cfg(test)]
mod test {
    #[derive(Debug)]
    struct Dummy;

    impl crate::SemaphoreState for Dummy {
        type Params = ();
        type Permit = ();

        fn acquire(&mut self, _params: Self::Params) -> Result<Self::Permit, Self::Params> {
            Ok(())
        }

        fn release(&mut self, _permit: Self::Permit) {}
    }

    #[test]
    fn debug() {
        let s = crate::Semaphore::new_fifo(Dummy);
        let s = std::format!("{s:?}");
        assert_eq!(s, "Semaphore { fifo: true, state: Dummy, .. }");
    }
}
