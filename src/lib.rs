use std::{
    future::poll_fn,
    mem::ManuallyDrop,
    sync::Mutex,
    task::{Poll, Waker},
};

use tokio::sync::Notify;

#[derive(Debug)]
pub struct Semaphore<State> {
    state: Mutex<StateWrapper<State>>,
    queue: Queue,
}

#[derive(Debug)]
struct StateWrapper<State> {
    inner: State,
    leader: LeaderState,
}

#[derive(Debug)]
enum LeaderState {
    Registered(Waker),
    Woken,
    Empty,
}

#[derive(Debug)]
struct Queue {
    // leader: DiatomicWaker,
    notify: Notify,
    fifo: bool,
}

impl Queue {
    fn new(fifo: bool) -> Self {
        let notify = Notify::new();
        Self {
            // leader: None,
            notify,
            fifo,
        }
    }

    fn notify<S>(&self, lock: &mut StateWrapper<S>) {
        let leader = core::mem::replace(&mut lock.leader, LeaderState::Woken);
        match leader {
            // wake the leader
            LeaderState::Registered(waker) => waker.wake(),
            // do nothing, it's up to the leader now
            LeaderState::Woken => {}
            // reset back to empty, notify the queue
            LeaderState::Empty => {
                lock.leader = LeaderState::Empty;

                if self.fifo {
                    self.notify.notify_one();
                } else {
                    self.notify.notify_last();
                }
            }
        }
    }

    async fn wait(&self) {
        self.notify.notified().await
    }
}

pub trait SemaphoreState {
    type Params;
    type Permit;
    fn permits_available(&self) -> bool;

    /// Acquire a permit given the params.
    /// This must always return Some if [`permits_available`](SemaphoreState::permits_available) returns true.
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
        let mut state = StateWrapper {
            inner: state,
            leader: LeaderState::Empty,
        };
        let queue = Queue::new(fifo);
        if state.inner.permits_available() {
            queue.notify(&mut state);
        }
        Self {
            state: Mutex::new(state),
            queue,
        }
    }

    /// This gives direct access to the state, be careful not to
    /// break any of your own state invariants.
    pub fn with_state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        let mut state = self.state.lock().unwrap();
        let res = f(&mut state.inner);
        if state.inner.permits_available() {
            // let leader = core::mem::replace(&mut state.leader, LeaderState::Woken);
            // match leader {
            //     // wake the leader
            //     LeaderState::Registered(waker) => waker.wake(),
            //     // do nothing, it's up to the leader now
            //     LeaderState::Woken => {}
            //     // reset back to empty, notify the queue
            //     LeaderState::Empty => {
            //         state.leader = LeaderState::Empty;
            //         self.queue.notify(&mut state)
            //     }
            // }
            self.queue.notify(&mut state);
        }
        res
    }

    pub async fn acquire(&self, params: State::Params) -> Permit<'_, State> {
        self.queue.wait().await;

        struct AcquireState<'a, S: SemaphoreState> {
            semaphore: &'a Semaphore<S>,
            params: Option<S::Params>,
        }

        impl<S: SemaphoreState> Drop for AcquireState<'_, S> {
            fn drop(&mut self) {
                // params are queue up, so we are the leader
                if let Some(_params) = self.params.take() {
                    let mut state = self.semaphore.state.lock().unwrap();
                    let leader = core::mem::replace(&mut state.leader, LeaderState::Empty);

                    // we must notify the next in the queue
                    if let LeaderState::Woken = leader {
                        self.semaphore.queue.notify(&mut state);
                    }
                }
            }
        }

        // we will never have to drop it - it should be dropped by acquire_permit.
        let mut acq = AcquireState {
            semaphore: self,
            params: Some(params),
        };
        poll_fn(move |cx| {
            let params = acq.params.take().unwrap();

            let mut state = acq.semaphore.state.lock().unwrap();
            state.leader = LeaderState::Empty;

            match state.inner.acquire(params) {
                Err(p) => {
                    state.leader = LeaderState::Registered(cx.waker().clone());
                    acq.params = Some(p);

                    Poll::Pending
                }
                Ok(permit) => {
                    if state.inner.permits_available() {
                        self.queue.notify(&mut state);
                    }

                    Poll::Ready(Permit {
                        sem: self,
                        permit: ManuallyDrop::new(permit),
                    })
                }
            }
        })
        .await
    }
}

#[derive(Debug)]
pub struct Permit<'a, State: SemaphoreState> {
    sem: &'a Semaphore<State>,
    // this is never dropped because it's returned to the semaphore on drop
    permit: ManuallyDrop<State::Permit>,
}

impl<State: SemaphoreState> Drop for Permit<'_, State> {
    fn drop(&mut self) {
        let mut state = self.sem.state.lock().unwrap();
        // Safety: only taken on drop.
        state
            .inner
            .release(unsafe { ManuallyDrop::take(&mut self.permit) });

        if state.inner.permits_available() {
            let leader = core::mem::replace(&mut state.leader, LeaderState::Woken);
            match leader {
                // wake the leader
                LeaderState::Registered(waker) => waker.wake(),
                // do nothing, it's up to the leader now
                LeaderState::Woken => {}
                // reset back to empty, notify the queue
                LeaderState::Empty => {
                    state.leader = LeaderState::Empty;
                    self.sem.queue.notify(&mut state)
                }
            }
        }
    }
}
