use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use lock_api::RawMutex;
use pin_list::{Node, NodeData};

use crate::closeable::{IsCloseable, Uncloseable};
use crate::order::Order;
use crate::{SemaphoreQueue, SemaphoreState};

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
        node: Node<PinQueue<S::Params, S::Permit, C>>,
        state: &'a Mutex<R, SemaphoreQueue<S, C, O>>,
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
                            state.state.release(permit);
                            state.check();
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
                    // Safety: If the semaphore is closed (meaning we have no queue)
                    // then there's no way this node could be queued in the queue,
                    // therefore it must be removed.
                    let (permit, ()) = unsafe { node.take_removed_unchecked() };
                    if let Ok(permit) = permit {
                        state.state.release(permit);
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
                        // Safety: an `Enqueue` outcome is only returned for an open queue.
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

        // Safety: Either there is no queue, then we are guaranteed to be removed from it
        // Or there was a queue, but we were removed from it anyway (protected_mut returned None).
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

impl<S: SemaphoreState + ?Sized, C: IsCloseable, O: Order> SemaphoreQueue<S, C, O> {
    /// Acquire a permit, or join the queue if one is not currently available.
    ///
    /// The queue's [`Order`] decides which end a new waiter joins; permits are
    /// always handed out from the front.
    #[inline]
    pub fn acquire<R: RawMutex>(
        this: &Mutex<R, Self>,
        params: S::Params,
    ) -> Acquire<'_, S, C, O, R> {
        Acquire {
            node: Node::new(),
            state: this,
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
        &mut self,
        params: S::Params,
        fair: bool,
    ) -> Result<S::Permit, TryAcquireError<S::Params, C>> {
        match self.acquire_or_enqueue(params, fair) {
            Outcome::Acquired(permit) => Ok(permit),
            Outcome::Closed(err) => Err(TryAcquireError::Closed(err)),
            Outcome::Poisoned(params) => Err(TryAcquireError::Poisoned(params)),
            Outcome::Enqueue { params, .. } => Err(TryAcquireError::NoPermits(params)),
        }
    }

    /// Grant a permit if we may, otherwise report which end the caller should queue
    /// at. Consults the [`Order`] exactly once, so a stateful order draws once per
    /// acquisition.
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
