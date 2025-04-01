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
//!
//! # Example
//!
//! ```
//!  #[derive(Debug)]
//! struct SemaphoreCounter(usize);
//!
//! impl flag_bearer::SemaphoreState for SemaphoreCounter {
//!     /// Number of permits to acquire
//!     type Params = usize;
//!
//!     /// Number of permits that have been acquired
//!     type Permit = usize;
//!
//!     fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params> {
//!         if let Some(available) = self.0.checked_sub(params) {
//!             self.0 = available;
//!             Ok(params)
//!         } else {
//!             Err(params)
//!         }
//!     }
//!
//!     fn release(&mut self, permit: Self::Permit) {
//!         self.0 = self.0.checked_add(permit).unwrap()
//!     }
//! }
//!
//! # #[tokio::main] async fn main() {
//! // create a new FIFO semaphore with 20 permits
//! let semaphore = flag_bearer::Semaphore::new_fifo(SemaphoreCounter(20));
//!
//! // acquire a token
//! let _permit = semaphore.acquire(1).await.expect("semaphore shouldn't be closed");
//!
//! // add 20 more permits
//! semaphore.with_state(|s| s.0 += 20);
//!
//! // release a token
//! drop(_permit);
//!
//! // close a semaphore
//! semaphore.close();
//! # }
//! ```

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
pub struct Semaphore<S: SemaphoreState + ?Sized> {
    order: FairOrder,
    state: Mutex<QueueState<S>>,
}

#[derive(Debug, Clone, Copy)]
enum FairOrder {
    /// Last in, first out.
    /// Increases tail latencies, but can have better average performance.
    Lifo,
    /// First in, first out.
    /// Fairer option, but can have cascading failures if queue processing is slow.
    Fifo,
}

impl FairOrder {
    fn is_lifo(&self) -> bool {
        matches!(self, FairOrder::Lifo)
    }
}

impl<S: SemaphoreState + core::fmt::Debug> core::fmt::Debug for Semaphore<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_struct("Semaphore");
        d.field("order", &self.order);
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
        Self::new_inner(state, FairOrder::Fifo)
    }

    /// Create a new last-in-first-out semaphore with the given initial state.
    pub fn new_lifo(state: S) -> Self {
        Self::new_inner(state, FairOrder::Lifo)
    }

    fn new_inner(state: S, order: FairOrder) -> Self {
        let state = QueueState {
            state,
            // Safety: during acquire, we ensure that nodes in this queue
            // will never attempt to use a different queue to read the nodes.
            queue: Some(PinList::new(unsafe { pin_list::id::DebugChecked::new() })),
        };
        Self {
            state: Mutex::new(state),
            order,
        }
    }
}

impl<S: SemaphoreState + ?Sized> Semaphore<S> {
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

    /// Close the semaphore.
    ///
    /// All tasks currently waiting to acquire a token will immediately stop.
    /// No new acquire attempts will succeed.
    pub fn close(&self) {
        let Some(mut queue) = self.state.lock().queue.take() else {
            return;
        };

        let mut cursor = queue.cursor_front_mut();
        while let Some(p) = cursor.protected_mut() {
            let params =
                p.0.take()
                    .expect("params should be in place. possibly the acquire method panicked");
            match cursor.remove_current(Err(params)) {
                Ok((_, waker)) => waker.wake(),
                // Safety: with protected_mut, we have just made sure it is in the list
                Err(_) => unsafe { unreachable_unchecked() },
            }
        }
        debug_assert!(queue.is_empty());
    }
}

// don't question the weird bounds here...
struct QueueState<
    S: SemaphoreState<Params = Params, Permit = Permit> + ?Sized,
    Params = <S as SemaphoreState>::Params,
    Permit = <S as SemaphoreState>::Permit,
> {
    queue: Option<PinList<PinQueue<Params, Permit>>>,
    state: S,
}

type PinQueue<Params, Permit> = dyn pin_list::Types<
        Id = pin_list::id::DebugChecked,
        // Some(params), waker -> Pending
        // None, waker -> Invalid state.
        Protected = (Option<Params>, Waker),
        // Ok(permit) -> Ready
        // Err(params) -> Closed
        Removed = Result<Permit, Params>,
        Unprotected = (),
    >;

impl<S: SemaphoreState + ?Sized> QueueState<S> {
    #[inline]
    fn check(&mut self) {
        let Some(queue) = &mut self.queue else { return };
        let mut leader = queue.cursor_front_mut();
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

/// The drop-guard for semaphore permits.
/// Will ensure the permit is released when dropped.
pub struct Permit<'a, S: SemaphoreState + ?Sized> {
    sem: &'a Semaphore<S>,
    // this is never dropped because it's returned to the semaphore on drop
    permit: ManuallyDrop<S::Permit>,
}

impl<S: SemaphoreState + ?Sized> core::fmt::Debug for Permit<'_, S>
where
    S::Permit: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Permit")
            .field("permit", &self.permit)
            .finish_non_exhaustive()
    }
}

impl<'a, S: SemaphoreState + ?Sized> Permit<'a, S> {
    /// Construct a new permit out of thin air, no waiting is required.
    ///
    /// This violates the purpose of the semahpore, but is provided for convenience.
    pub fn out_of_thin_air(sem: &'a Semaphore<S>, permit: S::Permit) -> Self {
        Self {
            sem,
            permit: ManuallyDrop::new(permit),
        }
    }

    /// Get read access to the permit value
    pub fn permit(&self) -> &S::Permit {
        &self.permit
    }

    /// Get mut access to the permit value
    ///
    /// It is up to the caller to maintain any semaphore invariants
    /// that might be violated when returning this permit.
    pub fn permit_mut(&mut self) -> &mut S::Permit {
        &mut self.permit
    }

    /// Get read access to the associated semaphore
    pub fn semaphore(&self) -> &'a Semaphore<S> {
        self.sem
    }

    /// Do not release the permit to the semaphore.
    pub fn take(self) -> S::Permit {
        let mut this = ManuallyDrop::new(self);
        unsafe { ManuallyDrop::take(&mut this.permit) }
    }
}

impl<S: SemaphoreState + ?Sized> Drop for Permit<'_, S> {
    fn drop(&mut self) {
        // Safety: only taken on drop.
        let permit = unsafe { ManuallyDrop::take(&mut self.permit) };
        self.sem.with_state(|s| s.release(permit));
    }
}

#[cfg(test)]
mod test {
    use std::sync::Arc;

    use crate::{Semaphore, SemaphoreState};

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
        assert_eq!(s, "Semaphore { order: Fifo, state: Dummy, .. }");
    }

    #[derive(Debug)]
    struct Counter(u64);

    impl crate::SemaphoreState for Counter {
        type Params = ();
        type Permit = ();

        fn acquire(&mut self, p: Self::Params) -> Result<Self::Permit, Self::Params> {
            let Some(n) = self.0.checked_sub(1) else {
                return Err(p);
            };
            self.0 = n;
            Ok(p)
        }

        fn release(&mut self, _: Self::Permit) {
            self.0 = self.0.saturating_add(1);
        }
    }

    #[tokio::test]
    async fn trait_object() {
        let s1: Arc<Semaphore<dyn SemaphoreState<Params = (), Permit = ()>>> =
            Arc::new(Semaphore::new_fifo(Dummy));

        let s2: Arc<Semaphore<dyn SemaphoreState<Params = (), Permit = ()>>> =
            Arc::new(Semaphore::new_fifo(Counter(1)));

        let _p1 = s1.acquire(()).await.unwrap();
        let _p2 = s2.acquire(()).await.unwrap();
    }
}
