use core::{hint::unreachable_unchecked, task::Waker};

use pin_list::PinList;

use crate::{Closeable, IsCloseable, SemaphoreState, closeable};

pub mod acquire;

// don't question the weird bounds here...
pub(crate) struct QueueState<
    S: SemaphoreState<Params = Params, Permit = Permit> + ?Sized,
    C: IsCloseable,
    Params = <S as SemaphoreState>::Params,
    Permit = <S as SemaphoreState>::Permit,
> {
    #[allow(clippy::type_complexity)]
    queue: Result<PinList<PinQueue<Params, Permit, C>>, C::Closed<()>>,
    pub(crate) state: S,
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
    /// Safety: Node's linked into this queue must only access against the same queue.
    pub(crate) unsafe fn new(state: S) -> Self {
        Self {
            state,
            // Safety: from caller
            queue: Ok(PinList::new(unsafe { pin_list::id::DebugChecked::new() })),
        }
    }
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable> QueueState<S, C> {
    #[inline]
    pub(crate) fn check(&mut self) {
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
    pub(crate) fn is_closed(&self) -> bool {
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
