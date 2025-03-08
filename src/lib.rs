use std::{mem::ManuallyDrop, task::Waker};

use parking_lot::Mutex;
use pin_list::PinList;

mod acquire;

pub struct Semaphore<State> {
    state: Mutex<StateWrapper<State>>,
    fifo: bool,
}

impl<State: SemaphoreState + std::fmt::Debug> std::fmt::Debug for Semaphore<State> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Semaphore");
        match self.state.try_lock() {
            Some(guard) => {
                d.field("state", &guard.inner);
            }
            None => {
                d.field("state", &format_args!("<locked>"));
            }
        }
        d.finish_non_exhaustive()
    }
}

struct StateWrapper<State> {
    inner: State,
    queue: PinList<PinQueue>,
}

type PinQueue = dyn pin_list::Types<
        Id = pin_list::id::DebugChecked,
        Protected = Waker,
        Removed = (),
        Unprotected = (),
    >;

pub trait SemaphoreState {
    type Params;
    type Permit;

    fn permits_available(&self) -> bool;

    /// Acquire a permit given the params.
    fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params>;

    fn release(&mut self, permit: Self::Permit);
}

impl<State: SemaphoreState> Semaphore<State> {
    pub fn new_fifo(state: State) -> Self {
        Self::new_inner(state, true)
    }

    pub fn new_lifo(state: State) -> Self {
        Self::new_inner(state, false)
    }

    fn new_inner(state: State, fifo: bool) -> Self {
        let state = StateWrapper {
            inner: state,
            queue: PinList::new(unsafe { pin_list::id::DebugChecked::new() }),
        };
        Self {
            state: Mutex::new(state),
            fifo,
        }
    }

    /// This gives direct access to the state, be careful not to
    /// break any of your own state invariants.
    pub fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self.state.lock();
        let res = f(&mut state.inner);
        if state.inner.permits_available() {
            // self.queue.notify(&mut state);
        }
        res
    }
}

#[derive(Debug)]
pub struct Permit<'a, State: SemaphoreState> {
    sem: &'a Semaphore<State>,
    // this is never dropped because it's returned to the semaphore on drop
    permit: ManuallyDrop<State::Permit>,
}

impl<'a, State: SemaphoreState> Permit<'a, State> {
    fn new(sem: &'a Semaphore<State>, permit: State::Permit) -> Self {
        Self {
            sem,
            permit: ManuallyDrop::new(permit),
        }
    }

    pub fn permit(&self) -> &State::Permit {
        &self.permit
    }
}

impl<State: SemaphoreState> Drop for Permit<'_, State> {
    fn drop(&mut self) {
        let mut state = self.sem.state.lock();
        // Safety: only taken on drop.
        state
            .inner
            .release(unsafe { ManuallyDrop::take(&mut self.permit) });

        if state.inner.permits_available() {
            // self.sem.queue.notify(&mut state)
        }
    }
}
