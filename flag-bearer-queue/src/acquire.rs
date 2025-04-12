use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use lock_api::RawMutex;
use pin_list::{Node, NodeData};

use crate::closeable::{IsCloseable, Uncloseable};
use crate::{SemaphoreQueue, SemaphoreState};

use super::PinQueue;

use crate::loom::Mutex;

pin_project_lite::pin_project! {
    /// A [`Future`] that acquires a permit from a [`SemaphoreQueue`].
    pub struct Acquire<'a, S, C, R>
    where
        S: ?Sized,
        S: SemaphoreState,
        C: IsCloseable,
        R: RawMutex,
    {
        #[pin]
        node: Node<PinQueue<S::Params, S::Permit, C>>,
        order: FairOrder,
        state: &'a Mutex<R, SemaphoreQueue<S, C>>,
        params: Option<S::Params>,
    }

    impl<S, C, R> PinnedDrop for Acquire<'_, S, C, R>
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
                    let (data, _unprotected) = node.reset(queue);
                    if let NodeData::Removed(Ok(permit)) = data {
                        state.state.release(permit);
                        state.check();
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

impl<S: SemaphoreState + ?Sized, C: IsCloseable, R: RawMutex> Future for Acquire<'_, S, C, R> {
    type Output = Result<S::Permit, C::AcquireError<S::Params>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let mut state = this.state.lock();

        let Some(init) = this.node.as_mut().initialized_mut() else {
            // first time polling.
            let params = this.params.take().unwrap();
            let node = this.node.as_mut();

            return match state.try_acquire(params, Fairness::Fair(*this.order)) {
                Ok(permit) => Poll::Ready(Ok(permit)),
                Err(TryAcquireError::Closed(params)) => Poll::Ready(Err(params)),
                Err(TryAcquireError::NoPermits(params)) => {
                    let queue = match &mut state.queue {
                        Ok(queue) => queue,
                        // Safety: if the queue was closed, we would get a `Closed` error type.
                        // It was not closed, thus it still isn't closed.
                        Err(_closed) => unsafe { core::hint::unreachable_unchecked() },
                    };

                    // no permit or we are not the leader, so we register into the queue.
                    let mut cursor = queue.cursor_ghost_mut();
                    let protected = (Some(params), cx.waker().clone());
                    let unprotected = ();
                    match *this.order {
                        FairOrder::Lifo => cursor.insert_after(node, protected, unprotected),
                        FairOrder::Fifo => cursor.insert_before(node, protected, unprotected),
                    };
                    Poll::Pending
                }
            };
        };

        if let Ok(queue) = &mut state.queue {
            if let Some((_, waker)) = init.protected_mut(queue) {
                // spurious wakeup
                waker.clone_from(cx.waker());
                return Poll::Pending;
            }
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

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
/// The order of which [`Acquire`] should enter the queue.
pub enum FairOrder {
    /// Last in, first out.
    /// Increases tail latencies, but can have better average performance.
    Lifo,
    /// First in, first out.
    /// Fairer option, but can have cascading failures if queue processing is slow.
    Fifo,
}

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
/// Which fairness property [`SemaphoreQueue::try_acquire`] should respect
pub enum Fairness {
    /// [`SemaphoreQueue::try_acquire`] will be fair.
    Fair(FairOrder),
    /// [`SemaphoreQueue::try_acquire`] will be unfair.
    Unfair,
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable> SemaphoreQueue<S, C> {
    /// Acquire a permit, or join the queue if not currently available.
    ///
    /// * If the order is [`FairOrder::Lifo`], then we enqueue at the front of the queue.
    /// * If the order is [`FairOrder::Fifo`], then we enqueue at the back of the queue.
    #[inline]
    pub fn acquire<R: RawMutex>(
        this: &Mutex<R, Self>,
        params: S::Params,
        order: FairOrder,
    ) -> Acquire<'_, S, C, R> {
        Acquire {
            node: Node::new(),
            order,
            state: this,
            params: Some(params),
        }
    }

    /// Try acquire a permit without joining the queue.
    ///
    /// * If the fairness is [`Fairness::Unfair`], or [`Fairness::Fair(FairOrder::Lifo)`](FairOrder::Lifo), then we always try acquire a permit.
    /// * If the fairness is [`Fairness::Fair(FairOrder::Fifo)`](FairOrder::Fifo), then we only try acquire a permit if the queue is empty.
    ///
    /// # Errors
    ///
    /// If there are currently not enough permits available for the given request,
    /// then [`TryAcquireError::NoPermits`] is returned.
    ///
    /// If this is a [`Fairness::Fair(FairOrder::Fifo)`](FairOrder::Fifo) semaphore queue,
    /// and there are other tasks waiting for permits,
    /// then [`TryAcquireError::NoPermits`] is returned.
    ///
    /// If this semaphore [`is_closed`](SemaphoreQueue::is_closed), then [`TryAcquireError::Closed`] is returned.
    #[inline]
    pub fn try_acquire(
        &mut self,
        params: S::Params,
        fairness: Fairness,
    ) -> Result<S::Permit, TryAcquireError<S::Params, C>> {
        let queue = match &mut self.queue {
            Ok(queue) => queue,
            Err(_closed) => {
                return Err(TryAcquireError::Closed(C::new_err(params)));
            }
        };

        let is_leader = match fairness {
            // if first-in-first-out, we are only the leader if the queue is empty.
            Fairness::Fair(FairOrder::Fifo) => queue.is_empty(),

            // if unfair, then we don't care who the leader is.
            // if last-in-first-out, we are the last in and thus the leader.
            Fairness::Unfair | Fairness::Fair(FairOrder::Lifo) => true,
        };

        if !is_leader {
            return Err(TryAcquireError::NoPermits(params));
        }

        match self.state.acquire(params) {
            Ok(permit) => Ok(permit),
            Err(p) => Err(TryAcquireError::NoPermits(p)),
        }
    }
}

/// The error returned by [`SemaphoreQueue::try_acquire`].
#[derive(Debug, PartialEq, Eq)]
pub enum TryAcquireError<P, C: IsCloseable> {
    /// The semaphore had no permits to give out right now.
    NoPermits(P),
    /// The semaphore is closed.
    Closed(C::AcquireError<P>),
}

impl<P, C: IsCloseable> fmt::Display for TryAcquireError<P, C> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TryAcquireError::Closed(_) => write!(fmt, "semaphore closed"),
            TryAcquireError::NoPermits(_) => write!(fmt, "no permits available"),
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
/// let s = flag_bearer::new_fifo().closeable().with_state(Counter(1));
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
