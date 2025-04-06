use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use pin_list::{Node, NodeData};

use crate::{FairOrder, IsCloseable, Permit, QueueState, Semaphore, SemaphoreState, Uncloseable};

use super::PinQueue;

impl<S: SemaphoreState + ?Sized> Semaphore<S, Uncloseable> {
    /// Acquire a new permit fairly with the given parameters.
    ///
    /// If a permit is not immediately available, this task will
    /// join a queue.
    pub async fn must_acquire(&self, params: S::Params) -> Permit<'_, S, Uncloseable> {
        self.acquire(params).await.unwrap_or_else(|e| e.never())
    }
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable> Semaphore<S, C> {
    #[inline]
    fn try_acquire_inner(
        &self,
        params: S::Params,
        fairness: Fairness,
    ) -> Result<S::Permit, TryAcquireError<S::Params, C>> {
        let mut state = self.state.lock();
        match state.unlinked_try_acquire(params, self.order, fairness) {
            Ok(permit) => Ok(permit),
            Err(params) => Err(params),
        }
    }

    /// Acquire a new permit fairly with the given parameters.
    ///
    /// If a permit is not immediately available, this task will
    /// join a queue.
    ///
    /// # Errors
    ///
    /// If this semaphore [`is_closed`](Semaphore::is_closed), then an [`AcquireError`] is returned.
    pub async fn acquire(
        &self,
        params: S::Params,
    ) -> Result<Permit<'_, S, C>, C::AcquireError<S::Params>> {
        let acquire = Acquire {
            sem: self,
            node: Node::new(),
            params: Some(params),
        };

        Ok(Permit::out_of_thin_air(self, acquire.await?))
    }

    /// Acquire a new permit fairly with the given parameters.
    ///
    /// If this is a LIFO semaphore, and there are other tasks waiting for permits,
    /// this will still try to acquire the permit - as this task would effectively
    /// be the last in the queue.
    ///
    /// # Errors
    ///
    /// If there are currently not enough permits available for the given request,
    /// then [`TryAcquireError::NoPermits`] is returned.
    ///
    /// If this is a FIFO semaphore, and there are other tasks waiting for permits,
    /// then [`TryAcquireError::NoPermits`] is returned.
    ///
    /// If this semaphore [`is_closed`](Semaphore::is_closed), then [`TryAcquireError::Closed`] is returned.
    pub fn try_acquire(
        &self,
        params: S::Params,
    ) -> Result<Permit<'_, S, C>, TryAcquireError<S::Params, C>> {
        let permit = self.try_acquire_inner(params, Fairness::Fair)?;
        Ok(Permit::out_of_thin_air(self, permit))
    }

    /// Acquire a new permit, potentially unfairly, with the given parameters.
    ///
    /// If this is a FIFO semaphore, and there are other tasks waiting for permits,
    /// this will still try to acquire the permit.
    ///
    /// # Errors
    ///
    /// If there are currently not enough permits available for the given request,
    /// then [`TryAcquireError::NoPermits`] is returned.
    ///
    /// If this semaphore [`is_closed`](Semaphore::is_closed), then [`TryAcquireError::Closed`] is returned.
    pub fn try_acquire_unfair(
        &self,
        params: S::Params,
    ) -> Result<Permit<'_, S, C>, TryAcquireError<S::Params, C>> {
        let permit = self.try_acquire_inner(params, Fairness::Unfair)?;
        Ok(Permit::out_of_thin_air(self, permit))
    }
}

pin_project_lite::pin_project! {
    struct Acquire<'a, S, C>
    where
        S: ?Sized,
        S: SemaphoreState,
        C: IsCloseable,
    {
        #[pin]
        node: Node<PinQueue<S::Params, S::Permit, C>>,
        sem: &'a Semaphore<S, C>,
        params: Option<S::Params>,
    }

    impl<S, C> PinnedDrop for Acquire<'_, S, C>
    where
        S: ?Sized,
        S: SemaphoreState,
        C: IsCloseable,
    {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let Some(node) = this.node.initialized_mut() else {
                return;
            };
            let mut state = this.sem.state.lock();
            match &mut state.queue {
                Ok(queue) => {
                    let (data, _unprotected) = node.reset(queue);
                    if let NodeData::Removed(Ok(permit)) = data {
                        state.state.release(permit);
                    }
                    state.check();
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

impl<S: SemaphoreState + ?Sized, C: IsCloseable> Future for Acquire<'_, S, C> {
    type Output = Result<S::Permit, C::AcquireError<S::Params>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let mut state = this.sem.state.lock();

        let Some(init) = this.node.as_mut().initialized_mut() else {
            // first time polling.
            let params = this.params.take().unwrap();
            let node = this.node.as_mut();

            match state.unlinked_try_acquire(params, this.sem.order, Fairness::Fair) {
                Ok(permit) => return Poll::Ready(Ok(permit)),
                Err(TryAcquireError::Closed(params)) => return Poll::Ready(Err(params)),
                Err(TryAcquireError::NoPermits(params)) => {
                    let queue = match &mut state.queue {
                        Ok(queue) => queue,
                        // Safety: if the queue was closed, we would get a `Closed` error type.
                        // It was not closed, thus it still isn't closed.
                        Err(_closed) => unsafe { core::hint::unreachable_unchecked() },
                    };

                    // no permit or we are not the leader, so we register into the queue.
                    let waker = cx.waker().clone();
                    match this.sem.order {
                        FairOrder::Lifo => queue.push_front(node, (Some(params), waker), ()),
                        FairOrder::Fifo => queue.push_back(node, (Some(params), waker), ()),
                    };
                    return Poll::Pending;
                }
            }
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

enum Fairness {
    Fair,
    Unfair,
}

impl Fairness {
    fn is_unfair(&self) -> bool {
        matches!(self, Fairness::Unfair)
    }
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable> QueueState<S, C> {
    #[inline]
    fn unlinked_try_acquire(
        &mut self,
        params: S::Params,
        order: FairOrder,
        fairness: Fairness,
    ) -> Result<S::Permit, TryAcquireError<S::Params, C>> {
        let queue = match &mut self.queue {
            Ok(queue) => queue,
            Err(_closed) => {
                return Err(TryAcquireError::Closed(C::new_err(params)));
            }
        };

        // if last-in-first-out, we are the last in and thus the leader.
        // if first-in-first-out, we are only the leader if the queue is empty.
        let is_leader = fairness.is_unfair() || order.is_lifo() || queue.is_empty();

        // if we are the leader, try and acquire a permit.
        if is_leader {
            match self.state.acquire(params) {
                Ok(permit) => Ok(permit),
                Err(p) => Err(TryAcquireError::NoPermits(p)),
            }
        } else {
            Err(TryAcquireError::NoPermits(params))
        }
    }
}

/// The error returned by [`try_acquire`](Semaphore::try_acquire)
///
/// ### NoPermits
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
/// let s = flag_bearer::SemaphoreBuilder::fifo().with_state(Counter(0));
///
/// match s.try_acquire(()) {
///     Err(flag_bearer::TryAcquireError::NoPermits(_)) => {},
///     _ => unreachable!(),
/// }
/// ```
///
/// ### Closed
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
/// let s = flag_bearer::SemaphoreBuilder::fifo().closeable().with_state(Counter(1));
///
/// // closing the semaphore makes all current and new acquire() calls return an error.
/// s.close();
///
/// match s.try_acquire(()) {
///     Err(flag_bearer::TryAcquireError::Closed(_)) => {},
///     _ => unreachable!(),
/// }
/// ```
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

/// The error returned by [`acquire`](Semaphore::acquire) if the semaphore was closed.
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
/// let s = flag_bearer::SemaphoreBuilder::fifo().closeable().with_state(Counter(1));
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
    /// Since the [`Semaphore`] is [`Uncloseable`], there can
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
