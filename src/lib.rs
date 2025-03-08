use std::{mem::ManuallyDrop, task::Waker};

use parking_lot::Mutex;
use pin_list::PinList;

mod acquire;
pub use acquire::TryAcquireError;

pub struct Semaphore<S: SemaphoreState> {
    state: Mutex<QueueState<S>>,
    fifo: bool,
}

impl<S: SemaphoreState + std::fmt::Debug> std::fmt::Debug for Semaphore<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Semaphore");
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

struct QueueState<S: SemaphoreState> {
    queue: PinList<PinQueue<S>>,
    state: S,
}

impl<S: SemaphoreState> QueueState<S> {
    fn check(&mut self, fifo: bool) {
        let mut leader = if fifo {
            self.queue.cursor_front_mut()
        } else {
            self.queue.cursor_back_mut()
        };

        let Some(p) = leader.protected_mut() else {
            return;
        };

        let params =
            p.0.take()
                .expect("params should be in place. possibly the acquire method panicked");
        match self.state.acquire(params) {
            Ok(permit) => match leader.remove_current(permit) {
                Ok((_, waker)) => waker.wake(),
                Err(_) => unreachable!("we have just made sure it is in the list"),
            },
            Err(params) => {
                p.0 = Some(params);
            }
        }
    }
}

type PinQueue<S> = dyn pin_list::Types<
        Id = pin_list::id::DebugChecked,
        Protected = (Option<<S as SemaphoreState>::Params>, Waker),
        Removed = <S as SemaphoreState>::Permit,
        Unprotected = (),
    >;

pub trait SemaphoreState {
    type Params;
    type Permit;

    /// Acquire a permit given the params.
    fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params>;

    fn release(&mut self, permit: Self::Permit);
}

impl<S: SemaphoreState> Semaphore<S> {
    pub fn new_fifo(state: S) -> Self {
        Self::new_inner(state, true)
    }

    pub fn new_lifo(state: S) -> Self {
        Self::new_inner(state, false)
    }

    fn new_inner(state: S, fifo: bool) -> Self {
        let state = QueueState {
            state,
            queue: PinList::new(unsafe { pin_list::id::DebugChecked::new() }),
        };
        Self {
            state: Mutex::new(state),
            fifo,
        }
    }

    /// This gives direct access to the state, be careful not to
    /// break any of your own state invariants.
    pub fn with_state<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let mut state = self.state.lock();
        let res = f(&mut state.state);
        state.check(self.fifo);
        res
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
}

impl<S: SemaphoreState> Drop for Permit<'_, S> {
    fn drop(&mut self) {
        // Safety: only taken on drop.
        let permit = unsafe { ManuallyDrop::take(&mut self.permit) };
        self.sem.with_state(|s| s.release(permit));
    }
}
