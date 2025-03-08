use std::{mem::ManuallyDrop, task::Waker};

use parking_lot::Mutex;
use pin_list::PinList;

mod acquire;

pub struct Semaphore<State: SemaphoreState> {
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

struct StateWrapper<State: SemaphoreState> {
    inner: State,
    queue: PinList<PinQueue<State>>,
}

impl<S: SemaphoreState> StateWrapper<S> {
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
        match self.inner.acquire(params) {
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
        state.check(self.fifo);
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
        // Safety: only taken on drop.
        let permit = unsafe { ManuallyDrop::take(&mut self.permit) };
        self.sem.with_state(|s| s.release(permit));
    }
}
