#![no_std]
#![warn(
    unsafe_op_in_unsafe_fn,
    clippy::missing_safety_doc,
    clippy::multiple_unsafe_ops_per_block,
    clippy::undocumented_unsafe_blocks
)]

#[cfg(test)]
extern crate std;

use core::{hint::unreachable_unchecked, task::Waker};

use crate::loom::Mutex;
use closeable::{Closeable, IsCloseable};
use flag_bearer_core::SemaphoreState;
use lock_api::RawMutex;
use pin_list::PinList;

pub mod acquire;
pub mod closeable;
pub mod order;

mod loom;

pub use order::{FairOrder, Hybrid, Order};

/// A queue that manages the acquisition of permits from a [`SemaphoreState`], or queues tasks
/// if no permits are available.
///
/// `R` selects the [`RawMutex`] implementation. The mutex and queue state are
/// private so an outstanding acquire always accesses its original list.
///
/// Replacing a borrowed queue is rejected at compile time:
///
/// ```compile_fail,E0502
/// use flag_bearer_core::SemaphoreState;
/// use flag_bearer_mutex::RawMutex;
/// use flag_bearer_queue::{SemaphoreQueue, FairOrder, closeable::Closeable};
/// # struct State;
/// # impl SemaphoreState for State {
/// #     type Params = ();
/// #     type Permit = ();
/// #     fn acquire(&mut self, _: ()) -> Result<(), ()> { Err(()) }
/// #     fn release(&mut self, _: ()) {}
/// # }
/// let mut queue = SemaphoreQueue::<RawMutex, _, Closeable>::new(State, FairOrder::Fifo);
/// let acquire = queue.acquire(());
/// let replacement = SemaphoreQueue::new(State, FairOrder::Fifo);
/// let original = core::mem::replace(&mut queue, replacement);
/// drop(acquire);
/// ```
///
/// The internal mutex cannot be used to replace the list either:
///
/// ```compile_fail,E0616
/// use flag_bearer_core::SemaphoreState;
/// use flag_bearer_mutex::RawMutex;
/// use flag_bearer_queue::{SemaphoreQueue, FairOrder, closeable::Closeable};
/// # struct State;
/// # impl SemaphoreState for State {
/// #     type Params = ();
/// #     type Permit = ();
/// #     fn acquire(&mut self, _: ()) -> Result<(), ()> { Err(()) }
/// #     fn release(&mut self, _: ()) {}
/// # }
/// let queue = SemaphoreQueue::<RawMutex, _, Closeable>::new(State, FairOrder::Fifo);
/// let guard = queue.state.lock();
/// ```
#[repr(transparent)]
pub struct SemaphoreQueue<R: RawMutex, S: SemaphoreState + ?Sized, C: IsCloseable, O = FairOrder> {
    // Safety invariant: Every usable initialized Acquire borrowing this mutex
    // originates from its list. The QueueState cannot be replaced through a
    // shared borrow, and an Err queue has no linked nodes.
    state: Mutex<R, QueueState<S, C, O>>,
}

impl<R: RawMutex, S: SemaphoreState, C: IsCloseable, O> SemaphoreQueue<R, S, C, O> {
    /// Construct a queue with its own mutex, state and ordering policy.
    #[inline]
    pub fn new(state: S, order: O) -> Self {
        Self {
            state: Mutex::new(QueueState {
                state,
                order,
                poisoned: false,
                len: 0,
                // SAFETY: DebugChecked IDs may only be compared within their
                // originating list. The private mutex never exposes QueueState;
                // Acquire borrows it and only registers/resets its own node in
                // this list. Public callbacks access S, not the list. Closing
                // removes all nodes before discarding the list, so no future
                // can reach a different list or compare IDs from another new().
                queue: Ok(PinList::new(unsafe { pin_list::id::DebugChecked::new() })),
            }),
        }
    }
}

impl<R: RawMutex, S: SemaphoreState + ?Sized, C: IsCloseable, O> SemaphoreQueue<R, S, C, O> {
    /// Inspect or modify the application state, then service queued acquires.
    #[inline]
    pub fn with_state<T>(&self, f: impl FnOnce(&mut S) -> T) -> T {
        self.state.lock().with_state(f)
    }

    /// Check whether this queue is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        C::CLOSE && self.state.lock().is_closed()
    }

    /// Check whether a state callback has poisoned the queue.
    #[inline]
    pub fn is_poisoned(&self) -> bool {
        self.state.lock().is_poisoned()
    }

    /// Clear poison after inspecting and repairing the application state.
    ///
    /// This does not itself service queued acquires. Follow it with a
    /// [`with_state`](Self::with_state) call to recheck existing waiters.
    #[inline]
    pub fn clear_poison(&self) {
        self.state.lock().clear_poison();
    }
}

impl<R: RawMutex, S: SemaphoreState + ?Sized, O> SemaphoreQueue<R, S, Closeable, O> {
    /// Close the queue, rejecting new requests and waking queued acquires.
    #[inline]
    pub fn close(&self) {
        self.state.lock().close();
    }
}

impl<R: RawMutex, S: SemaphoreState + core::fmt::Debug, C: IsCloseable, O> core::fmt::Debug
    for SemaphoreQueue<R, S, C, O>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.state.try_lock() {
            Some(state) => state.fmt(f),
            None => f.write_str("<locked>"),
        }
    }
}

// don't question the weird bounds here...
struct QueueState<
    S: SemaphoreState<Params = Params, Permit = Permit> + ?Sized,
    C: IsCloseable,
    O = FairOrder,
    Params = <S as SemaphoreState>::Params,
    Permit = <S as SemaphoreState>::Permit,
> {
    #[allow(clippy::type_complexity)]
    // Safety invariant: Initialized acquires borrowing the containing mutex
    // access only this list. Err is stored only after all nodes are removed;
    // an open list is never replaced with another list.
    queue: Result<PinList<PinQueue<Params, Permit, C>>, C::Closed<()>>,
    /// Set if a panic ever escaped user code (`SemaphoreState::acquire`,
    /// `SemaphoreState::release` or a `with_state` closure) while we held the state,
    /// which may have left `state`
    /// half-updated. We can't tell a clean panic from a corrupting one, so — like
    /// [`std::sync::Mutex`] — we assume the worst until `clear_poison`.
    poisoned: bool,
    /// Number of tasks currently waiting in `queue`. Tracked here because the
    /// intrusive list has no `len()`, and size-dependent [`Order`]s need it.
    len: usize,
    /// The ordering policy. A stateful policy (e.g. a randomized one) lives here so
    /// it's shared across acquisitions and mutated under the lock.
    order: O,
    /// `state` must stay the last field so it can be `?Sized`.
    state: S,
}

/// Sets the poison flag if dropped. Drop only runs if we unwind out of the
/// guarded user call; [`core::mem::forget`] it on the success path so a clean
/// call leaves the flag untouched (and never *clears* a prior poison).
pub(crate) struct PoisonOnUnwind<'a>(pub(crate) &'a mut bool);

impl Drop for PoisonOnUnwind<'_> {
    fn drop(&mut self) {
        *self.0 = true;
    }
}

impl<S: SemaphoreState + core::fmt::Debug, C: IsCloseable, O> core::fmt::Debug
    for QueueState<S, C, O>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_struct("SemaphoreQueue");
        d.field("state", &self.state);
        d.finish_non_exhaustive()
    }
}

type PinQueue<Params, Permit, C> = dyn pin_list::Types<
        Id = pin_list::id::DebugChecked,
        Protected = (
            // Some(params) -> Pending
            // None -> the leader's params have been taken: transiently while
            //         check() acquires, or left behind if that acquire panicked
            //         (in which case the queue is also poisoned).
            Option<Params>,
            Waker,
        ),
        Removed = Result<
            // Ok(permit) -> Ready
            Permit,
            // Err(Some(params)) -> Closed
            // Err(None) -> Closed, Invalid state
            <C as closeable::private::Sealed>::Closed<Option<Params>>,
        >,
        Unprotected = (),
    >;

impl<S: SemaphoreState + ?Sized, C: IsCloseable, O> QueueState<S, C, O> {
    /// Access the state with mutable access.
    ///
    /// This gives direct access to the state, be careful not to
    /// break any of your own state invariants. You can use this
    /// to peek at the current state, or to modify it, eg to add or
    /// remove permits from the semaphore.
    pub fn with_state<T>(&mut self, f: impl FnOnce(&mut S) -> T) -> T {
        // A panic in `f` may leave `state` half-updated, so poison if it unwinds.
        let guard = PoisonOnUnwind(&mut self.poisoned);
        let res = f(&mut self.state);
        core::mem::forget(guard);

        self.check();
        res
    }

    #[inline]
    fn check(&mut self) {
        if self.poisoned {
            return;
        }
        let Ok(queue) = &mut self.queue else { return };
        let mut leader = queue.cursor_front_mut();
        while let Some(p) = leader.protected_mut() {
            let Some(params) = p.0.take() else {
                // This node's params were lost to a panic in `acquire`. While
                // poisoned, the check above returns early, so we only reach here
                // after `clear_poison`. The waiter can never be satisfied (and
                // leaves when its future is dropped), so skip it and keep serving
                // the rest of the queue.
                leader.move_next();
                continue;
            };
            // A panic in `acquire` loses `params` and may leave `state`
            // half-updated, so poison if it unwinds.
            let guard = PoisonOnUnwind(&mut self.poisoned);
            let result = self.state.acquire(params);
            core::mem::forget(guard);

            match result {
                Ok(permit) => match leader.remove_current(Ok(permit)) {
                    Ok((_, waker)) => {
                        self.len -= 1;
                        waker.wake();
                    }
                    // SAFETY: protected_mut established that the cursor is on
                    // a node. acquire can mutate only S and cannot replace the
                    // private list; remove_current therefore cannot fail.
                    Err(_) => unsafe { unreachable_unchecked() },
                },
                Err(params) => {
                    p.0 = Some(params);
                    break;
                }
            }
        }
    }

    /// Check if the queue is closed
    pub fn is_closed(&self) -> bool {
        self.queue.is_err()
    }

    /// Check if the queue has been poisoned.
    ///
    /// A queue becomes poisoned if a panic unwinds out of [`SemaphoreState::acquire`],
    /// [`SemaphoreState::release`] or a [`with_state`](Self::with_state) closure,
    /// which may have left the state
    /// half-updated. Poisoning persists until [`clear_poison`](Self::clear_poison).
    /// A poisoned queue stops granting permits; new acquire attempts surface the
    /// poison rather than build on a corrupt state.
    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Clear the poison flag, letting the queue grant permits again.
    ///
    /// The caller is responsible for ensuring the state is consistent first (e.g.
    /// inspect/repair it with [`with_state`](Self::with_state)); like
    /// [`std::sync::Mutex::clear_poison`], this does not itself touch the state.
    ///
    /// A blocking acquire whose `acquire` impl panicked lost its params and can
    /// never complete; it is skipped until its future is dropped.
    pub fn clear_poison(&mut self) {
        self.poisoned = false;
    }
}

impl<S: SemaphoreState + ?Sized, O> QueueState<S, Closeable, O> {
    /// Close the semaphore queue.
    ///
    /// All tasks currently waiting to acquire a token will immediately stop.
    /// No new acquire attempts will succeed.
    ///
    /// # Safety-usable invariant
    ///
    /// Err is stored in queue only after every linked node has entered the
    /// removed state. If a waker panics, queue remains Ok and pin-list's
    /// fallback still leaves the affected node removed.
    pub fn close(&mut self) {
        let Ok(queue) = &mut self.queue else {
            return;
        };

        let mut cursor = queue.cursor_front_mut();
        while cursor.remove_current_with_or(
            |(params, waker)| {
                // The node is already unlinked when this closure runs. Keep
                // the count consistent even if the caller's waker panics.
                self.len -= 1;
                waker.wake();

                Err(params)
            },
            || Err(None),
        ) {}

        debug_assert!(queue.is_empty());

        // It's important that we only mark the queue as closed when we have ensured that
        // all linked nodes are removed.
        // If we did this early, we could panic and not dequeue every node.
        self.queue = Err(());
        debug_assert_eq!(self.len, 0);
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use crate::{SemaphoreQueue, closeable::Closeable};

    #[derive(Debug)]
    struct NeverSucceeds;

    impl crate::SemaphoreState for NeverSucceeds {
        type Params = ();
        type Permit = ();

        fn acquire(&mut self, _params: Self::Params) -> Result<Self::Permit, Self::Params> {
            Err(())
        }

        fn release(&mut self, _permit: Self::Permit) {}
    }

    #[test]
    fn concurrent_closed() {
        loom::model(|| {
            use std::sync::Arc;

            let s = Arc::new(SemaphoreQueue::<
                parking_lot::RawMutex,
                NeverSucceeds,
                Closeable,
            >::new(NeverSucceeds, crate::FairOrder::Fifo));

            let s2 = s.clone();
            let handle = loom::thread::spawn(move || {
                loom::future::block_on(async move {
                    SemaphoreQueue::acquire(&s2, ()).await.unwrap_err()
                })
            });

            s.close();

            handle.join().unwrap();
        });
    }
}
