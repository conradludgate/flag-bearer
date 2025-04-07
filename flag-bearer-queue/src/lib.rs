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

use flag_bearer_core::SemaphoreState;
use pin_list::PinList;

pub mod acquire;
mod closeable;
mod mutex;

pub use acquire::{Acquire, AcquireError, TryAcquireError};
pub use closeable::{Closeable, IsCloseable, Uncloseable};
use mutex::Mutex;

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum FairOrder {
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

// don't question the weird bounds here...
pub struct QueueState<
    S: SemaphoreState<Params = Params, Permit = Permit> + ?Sized,
    C: IsCloseable,
    Params = <S as SemaphoreState>::Params,
    Permit = <S as SemaphoreState>::Permit,
> {
    #[allow(clippy::type_complexity)]
    queue: Result<PinList<PinQueue<Params, Permit, C>>, C::Closed<()>>,
    pub state: S,
}

type PinQueue<Params, Permit, C> = dyn pin_list::Types<
        Id = pin_list::id::DebugChecked,
        Protected = (
            // Some(params) -> Pending
            // None -> Invalid state.
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

impl<S: SemaphoreState, C: IsCloseable> QueueState<S, C> {
    pub fn new(state: S) -> Self {
        Self {
            state,
            // Safety: during acquire, we ensure that nodes in this queue
            // will never attempt to use a different queue to read the nodes.
            queue: Ok(PinList::new(unsafe { pin_list::id::DebugChecked::new() })),
        }
    }
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable> QueueState<S, C> {
    #[inline]
    pub fn check(&mut self) {
        let Ok(queue) = &mut self.queue else { return };
        let mut leader = queue.cursor_front_mut();
        while let Some(p) = leader.protected_mut() {
            let params = p.0.take().expect(
                "params should be in place. possibly the SemaphoreState::acquire method panicked",
            );
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

    /// Check if the queue is closed
    pub fn is_closed(&self) -> bool {
        self.queue.is_err()
    }
}

impl<S: SemaphoreState + ?Sized> QueueState<S, Closeable> {
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
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use crate::{Acquire, Closeable, QueueState};

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

            let s = Arc::new(crate::Mutex::<parking_lot::RawMutex, _>::new(
                QueueState::<NeverSucceeds, Closeable>::new(NeverSucceeds),
            ));

            let s2 = s.clone();
            let handle = loom::thread::spawn(move || {
                loom::future::block_on(async move {
                    Acquire::new(&*s2, crate::FairOrder::Fifo, ())
                        .await
                        .unwrap_err()
                })
            });

            s.lock().close();

            handle.join().unwrap();
        });
    }
}
