use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use lock_api::RawMutex;
use pin_list::{Node, NodeData};

use crate::closeable::{IsCloseable, Uncloseable};
use crate::order::Order;
use crate::{QueueState, SemaphoreQueue, SemaphoreState};

use super::PinQueue;

use crate::loom::Mutex;

pin_project_lite::pin_project! {
    /// A [`Future`] that acquires a permit from a [`SemaphoreQueue`].
    pub struct Acquire<'a, S, C, O, R>
    where
        S: ?Sized,
        S: SemaphoreState,
        C: IsCloseable,
        R: RawMutex,
    {
        #[pin]
        // Safety invariant: While initialized, this node is linked to or has
        // been removed from state's original list. No other list is used to
        // inspect or reset it. QueueState::close removes all linked nodes
        // before discarding that list.
        node: Node<PinQueue<S::Params, S::Permit, C>>,
        state: &'a Mutex<R, QueueState<S, C, O>>,
        params: Option<S::Params>,
    }

    impl<S, C, O, R> PinnedDrop for Acquire<'_, S, C, O, R>
    where
        S: ?Sized,
        S: SemaphoreState,
        C: IsCloseable,
        R: RawMutex,
    {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let Some(node) = this.node.initialized_mut() else {
                return;
            };
            let mut state = this.state.lock();
            match &mut state.queue {
                Ok(queue) => {
                    match node.reset(queue).0 {
                        // We were granted a permit but never claimed it; return it.
                        NodeData::Removed(Ok(permit)) => {
                            state.with_state(|state| state.release(permit));
                        }
                        // We were still queued and have now left the queue. If we were
                        // a head-of-line blocker, the waiters behind us may now be
                        // serviceable, so re-check.
                        NodeData::Linked(_) => {
                            state.len -= 1;
                            state.check();
                        }
                        NodeData::Removed(Err(_closed)) => {}
                    }
                }
                Err(_closed) => {
                    // SAFETY: By Acquire::node's invariant this initialized
                    // node originates from this mutex's list. The private mutex
                    // prevents replacing QueueState, and QueueState::close's
                    // safety-usable invariant ensures all nodes are removed
                    // before queue becomes Err. Taking leaves node initial,
                    // so it cannot be taken or dropped again as initialized.
                    let (permit, ()) = unsafe { node.take_removed_unchecked() };
                    if let Ok(permit) = permit {
                        state.with_state(|state| state.release(permit));
                    }
                }
            }
        }
    }
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable, O: Order, R: RawMutex> Future
    for Acquire<'_, S, C, O, R>
{
    type Output = Result<S::Permit, C::AcquireError<S::Params>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let mut state = this.state.lock();

        let Some(init) = this.node.as_mut().initialized_mut() else {
            // first time polling.
            let params = this.params.take().unwrap();
            let node = this.node.as_mut();

            match state.acquire_or_enqueue(params, true) {
                Outcome::Acquired(permit) => return Poll::Ready(Ok(permit)),
                Outcome::Closed(err) => return Poll::Ready(Err(err)),
                // The async acquire's error type can't carry poison (it is
                // uninhabited for uncloseable semaphores, keeping `must_acquire`
                // infallible), so poison is a panic on this path. Callers that
                // want to observe poison should use `try_acquire`.
                Outcome::Poisoned(_params) => panic!(
                    "the semaphore is poisoned: a previous SemaphoreState::acquire call panicked"
                ),
                Outcome::Enqueue { front, params } => {
                    let queue = match &mut state.queue {
                        Ok(queue) => queue,
                        // SAFETY: acquire_or_enqueue's safety-usable invariant
                        // guarantees Enqueue leaves queue open, and no code
                        // between that call and this match can change it.
                        Err(_closed) => unsafe { core::hint::unreachable_unchecked() },
                    };

                    let waker = cx.waker().clone();
                    if front {
                        queue.push_front(node, (Some(params), waker), ());
                    } else {
                        queue.push_back(node, (Some(params), waker), ());
                    }
                    state.len += 1;
                    return Poll::Pending;
                }
            }
        };

        if let Ok(queue) = &mut state.queue
            && let Some((_, waker)) = init.protected_mut(queue)
        {
            // spurious wakeup
            waker.clone_from(cx.waker());
            return Poll::Pending;
        }

        // SAFETY: Acquire::node's invariant ties this node to the original
        // list in this private mutex. With an open queue, protected_mut returned
        // None, which pin-list guarantees means removed. With a closed queue,
        // QueueState::close's safety-usable invariant establishes removal.
        // The mutex excludes concurrent transitions. Taking leaves node initial
        // before user code runs or poll returns, preventing a second extraction.
        let (permit, ()) = unsafe { init.take_removed_unchecked() };
        let permit = permit.map_err(|params| {
            C::map_err(params, |params| {
                params.expect(
                    "params should be set. likely the SemaphoreState::acquire method panicked",
                )
            })
        });
        Poll::Ready(permit)
    }
}

/// The result of an acquisition attempt that doesn't itself touch the queue list.
enum Outcome<S: SemaphoreState + ?Sized, C: IsCloseable> {
    /// A permit was granted immediately.
    Acquired(S::Permit),
    /// The semaphore is closed.
    Closed(C::AcquireError<S::Params>),
    /// The semaphore is poisoned.
    Poisoned(S::Params),
    /// No permit available now; the caller should enqueue at the indicated end.
    Enqueue { front: bool, params: S::Params },
}

impl<R: RawMutex, S: SemaphoreState + ?Sized, C: IsCloseable, O: Order> SemaphoreQueue<R, S, C, O> {
    /// Acquire a permit, or join the queue if one is not currently available.
    ///
    /// The queue's [`Order`] decides which end a new waiter joins; permits are
    /// always handed out from the front.
    #[inline]
    pub fn acquire(&self, params: S::Params) -> Acquire<'_, S, C, O, R> {
        Acquire {
            node: Node::new(),
            state: &self.state,
            params: Some(params),
        }
    }

    /// Try to acquire a permit without joining the queue.
    ///
    /// When `fair`, the queue's [`Order`] decides whether this caller may take a
    /// permit ahead of any waiters — it can if the order would place it at the
    /// front (e.g. LIFO), or if the queue is empty. When not `fair`, it always tries.
    #[inline]
    pub fn try_acquire(
        &self,
        params: S::Params,
        fair: bool,
    ) -> Result<S::Permit, TryAcquireError<S::Params, C>> {
        let mut state = self.state.lock();
        let permit = match state.acquire_or_enqueue(params, fair) {
            Outcome::Acquired(permit) => permit,
            Outcome::Closed(err) => return Err(TryAcquireError::Closed(err)),
            Outcome::Poisoned(params) => return Err(TryAcquireError::Poisoned(params)),
            Outcome::Enqueue { params, .. } => return Err(TryAcquireError::NoPermits(params)),
        };
        Ok(permit)
    }
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable, O: Order> QueueState<S, C, O> {
    /// Grant a permit if we may, otherwise report which end the caller should queue
    /// at. Consults the [`Order`] exactly once, so a stateful order draws once per
    /// acquisition.
    ///
    /// # Safety-usable invariant
    ///
    /// Enqueue is returned only while queue is Ok. This method never replaces
    /// the list or changes its identity, including if State or Order panics.
    fn acquire_or_enqueue(&mut self, params: S::Params, fair: bool) -> Outcome<S, C> {
        if self.poisoned {
            return Outcome::Poisoned(params);
        }
        let empty = match &self.queue {
            Ok(queue) => queue.is_empty(),
            Err(_closed) => return Outcome::Closed(C::new_err(params)),
        };

        // A single ordering decision: would we go to the front, and are we therefore
        // the leader (front, or nobody ahead of us)? Unfair callers always try.
        let front = if fair {
            self.order.enqueue_front(self.len)
        } else {
            true
        };
        if !(front || empty) {
            return Outcome::Enqueue { front, params };
        }

        // We're the leader. A panic in `acquire` may leave `state` half-updated,
        // so poison if it unwinds.
        let guard = crate::PoisonOnUnwind(&mut self.poisoned);
        let result = self.state.acquire(params);
        core::mem::forget(guard);
        match result {
            Ok(permit) => Outcome::Acquired(permit),
            Err(params) => Outcome::Enqueue { front, params },
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TryAcquireError<P, C: IsCloseable> {
    /// The semaphore had no permits to give out right now.
    NoPermits(P),
    /// The semaphore is closed.
    Closed(C::AcquireError<P>),
    /// The semaphore is poisoned: a previous [`SemaphoreState::acquire`](crate::SemaphoreState::acquire)
    /// call panicked. The params are handed back unused.
    Poisoned(P),
}

impl<P, C: IsCloseable> fmt::Display for TryAcquireError<P, C> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryAcquireError::Closed(_) => write!(fmt, "semaphore closed"),
            TryAcquireError::NoPermits(_) => write!(fmt, "no permits available"),
            TryAcquireError::Poisoned(_) => write!(fmt, "semaphore poisoned"),
        }
    }
}

/// The error returned by [`Acquire`] if the semaphore queue was closed.
///
/// ```
/// struct Counter(usize);
///
/// impl flag_bearer::SemaphoreState for Counter {
///     type Params = ();
///     type Permit = ();
///
///     fn acquire(&mut self, _: Self::Params) -> Result<Self::Permit, Self::Params> {
///         if self.0 > 0 {
///             self.0 -= 1;
///             Ok(())
///         } else {
///             Err(())
///         }
///     }
///
///     fn release(&mut self, _: Self::Permit) {
///         self.0 += 1;
///     }
/// }
///
/// # pollster::block_on(async move {
/// let s = flag_bearer::Builder::fifo().closeable().with_state(Counter(1));
///
/// // closing the semaphore makes all current and new acquire() calls return an error.
/// s.close();
///
/// let _err = s.acquire(()).await.unwrap_err();
/// # });
/// ```
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq)]
pub struct AcquireError<P> {
    /// The params that was used in the acquire request
    pub params: P,
}

impl AcquireError<Uncloseable> {
    /// Since the [`SemaphoreQueue`] is [`Uncloseable`], there can
    /// never be an acquire error. This allows for unwrapping with type-safety.
    pub fn never(self) -> ! {
        match self.params {}
    }
}

impl<P> fmt::Display for AcquireError<P> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "semaphore closed")
    }
}
