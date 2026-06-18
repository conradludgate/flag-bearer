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

use closeable::{Closeable, IsCloseable};
use flag_bearer_core::SemaphoreState;
use pin_list::PinList;

pub mod acquire;
pub mod closeable;
pub mod order;

mod loom;

pub use order::{FairOrder, Order};

/// A queue that manages the acquisition of permits from a [`SemaphoreState`], or queues tasks
/// if no permits are available.
// don't question the weird bounds here...
pub struct SemaphoreQueue<
    S: SemaphoreState<Params = Params, Permit = Permit> + ?Sized,
    C: IsCloseable,
    O = FairOrder,
    Params = <S as SemaphoreState>::Params,
    Permit = <S as SemaphoreState>::Permit,
> {
    #[allow(clippy::type_complexity)]
    queue: Result<PinList<PinQueue<Params, Permit, C>>, C::Closed<()>>,
    /// Set if a panic ever escaped user code (`SemaphoreState::acquire` or a
    /// `with_state` closure) while we held the state, which may have left `state`
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
    for SemaphoreQueue<S, C, O>
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

impl<S: SemaphoreState, C: IsCloseable, O> SemaphoreQueue<S, C, O> {
    /// Construct a new semaphore queue, with the given [`SemaphoreState`] and [`Order`].
    pub fn new(state: S, order: O) -> Self {
        Self {
            state,
            order,
            poisoned: false,
            len: 0,
            // Safety: during acquire, we ensure that nodes in this queue
            // will never attempt to use a different queue to read the nodes.
            queue: Ok(PinList::new(unsafe { pin_list::id::DebugChecked::new() })),
        }
    }
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable, O> SemaphoreQueue<S, C, O> {
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
                        waker.wake();
                        self.len -= 1;
                    }
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

    /// Check if the queue is closed
    pub fn is_closed(&self) -> bool {
        self.queue.is_err()
    }

    /// Check if the queue has been poisoned.
    ///
    /// A queue becomes poisoned if a panic unwinds out of [`SemaphoreState::acquire`]
    /// or a [`with_state`](Self::with_state) closure, which may have left the state
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

impl<S: SemaphoreState + ?Sized, O> SemaphoreQueue<S, Closeable, O> {
    /// Close the semaphore queue.
    ///
    /// All tasks currently waiting to acquire a token will immediately stop.
    /// No new acquire attempts will succeed.
    pub fn close(&mut self) {
        let Ok(queue) = &mut self.queue else {
            return;
        };

        let mut cursor = queue.cursor_front_mut();
        while cursor.remove_current_with_or(
            |(params, waker)| {
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
        self.len = 0;
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

            let s = Arc::new(crate::loom::Mutex::<parking_lot::RawMutex, _>::new(
                SemaphoreQueue::<NeverSucceeds, Closeable>::new(
                    NeverSucceeds,
                    crate::FairOrder::Fifo,
                ),
            ));

            let s2 = s.clone();
            let handle = loom::thread::spawn(move || {
                loom::future::block_on(async move {
                    SemaphoreQueue::acquire(&s2, ()).await.unwrap_err()
                })
            });

            s.lock().close();

            handle.join().unwrap();
        });
    }
}
