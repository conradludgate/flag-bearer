use std::{
    future::poll_fn,
    mem::ManuallyDrop,
    pin::pin,
    sync::{Mutex, TryLockError},
    task::{Poll, Waker, ready},
};

use tokio::sync::{Notify, futures::Notified};

pub struct Semaphore<State: SemaphoreState> {
    state: Mutex<StateWrapper<State>>,
    queue: Queue,
}

impl<State: SemaphoreState + std::fmt::Debug> std::fmt::Debug for Semaphore<State> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("Semaphore");
        match self.state.try_lock() {
            Ok(guard) => {
                d.field("state", &guard.inner);
            }
            Err(TryLockError::Poisoned(err)) => {
                d.field("state", &err.get_ref().inner);
            }
            Err(TryLockError::WouldBlock) => {
                d.field("state", &format_args!("<locked>"));
            }
        }
        d.finish_non_exhaustive()
    }
}

struct StateWrapper<State> {
    inner: State,
    leader: LeaderState,
}

enum LeaderState {
    Registered(Waker),
    Woken,
    Empty,
}

#[derive(Debug)]
struct Queue {
    notify: Notify,
    fifo: bool,
}

impl Queue {
    fn new(fifo: bool) -> Self {
        let notify = Notify::new();
        Self { notify, fifo }
    }

    fn notify<S>(&self, lock: &mut StateWrapper<S>) {
        let leader = core::mem::replace(&mut lock.leader, LeaderState::Empty);
        match leader {
            // wake the leader
            LeaderState::Registered(waker) => {
                lock.leader = LeaderState::Woken;
                waker.wake()
            }
            // do nothing, it's up to the leader now
            LeaderState::Woken => {
                lock.leader = LeaderState::Woken;
            }
            // reset back to empty, notify the queue
            LeaderState::Empty => {
                if self.fifo {
                    self.notify.notify_one();
                } else {
                    self.notify.notify_last();
                }
            }
        }
    }

    fn wait(&self) -> Notified<'_> {
        self.notify.notified()
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
        let mut notified = pin!(Some(self.queue.wait()));
        let mut params = Some(params);

        poll_fn(|cx| {
            let mut state;
            if let Some(n) = notified.as_mut().as_pin_mut() {
                ready!(n.poll(cx));
                notified.set(None);

                state = self.state.lock().unwrap();
                state.leader = LeaderState::Woken;
            } else {
                state = self.state.lock().unwrap();
            }

            match core::mem::replace(&mut state.leader, LeaderState::Empty) {
                // spurious wakeup
                LeaderState::Registered(mut waker) => {
                    if !cx.waker().will_wake(&waker) {
                        waker = cx.waker().clone();
                    }
                    state.leader = LeaderState::Registered(waker);
                    Poll::Pending
                }
                // woken
                LeaderState::Woken => match state.inner.acquire(params.take().unwrap()) {
                    Err(p) => {
                        state.leader = LeaderState::Registered(cx.waker().clone());
                        params = Some(p);
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
                },
                LeaderState::Empty => unreachable!(),
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

impl<State: SemaphoreState> Permit<'_, State> {
    pub fn permit(&self) -> &State::Permit {
        &self.permit
    }
}

impl<State: SemaphoreState> Drop for Permit<'_, State> {
    fn drop(&mut self) {
        let mut state = self.sem.state.lock().unwrap();
        // Safety: only taken on drop.
        state
            .inner
            .release(unsafe { ManuallyDrop::take(&mut self.permit) });

        if state.inner.permits_available() {
            self.sem.queue.notify(&mut state)
        }
    }
}
