use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use pin_list::{Node, NodeData};

use crate::{FairOrder, IsCloseable, Permit, PinQueue, QueueState, Semaphore, SemaphoreState};

impl<S: SemaphoreState + ?Sized, C: IsCloseable> Semaphore<S, C> {
    #[inline]
    fn acquire_inner(
        &self,
        params: S::Params,
    ) -> impl Future<Output = Result<S::Permit, C::Closed<S::Params>>> {
        Acquire {
            sem: self,
            node: Node::new(),
            params: Some(params),
        }
    }

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
    pub async fn acquire(
        &self,
        params: S::Params,
    ) -> Result<Permit<'_, S, C>, C::Closed<AcquireError<S::Params>>> {
        let permit = self
            .acquire_inner(params)
            .await
            .map_err(|params| C::map(params, |params| AcquireError { params }))?;
        Ok(Permit::out_of_thin_air(self, permit))
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
    pub fn try_acquire_unfair(
        &self,
        params: S::Params,
    ) -> Result<Permit<'_, S, C>, TryAcquireError<S::Params, C>> {
        let permit = self.try_acquire_inner(params, Fairness::Unfair)?;
        Ok(Permit::out_of_thin_air(self, permit))
    }
}

struct Acquire<'a, S: SemaphoreState + ?Sized, C: IsCloseable> {
    // #[pin]
    node: Node<PinQueue<S::Params, S::Permit, C>>,
    sem: &'a Semaphore<S, C>,
    params: Option<S::Params>,
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable> Acquire<'_, S, C> {
    fn pinned_drop(this: Pin<&mut Self>) {
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
                let (permit, ()) = unsafe { node.take_removed_unchecked() };
                if let Ok(permit) = permit {
                    state.state.release(permit);
                }
            }
        }
    }
}

const _: () = {
    #[allow(clippy::type_complexity)]
    struct Projection<'__pin, 'a, S: SemaphoreState + ?Sized, C: IsCloseable>
    where
        Acquire<'a, S, C>: '__pin,
    {
        sem: &'__pin mut &'a Semaphore<S, C>,
        node: Pin<&'__pin mut Node<PinQueue<S::Params, S::Permit, C>>>,
        params: &'__pin mut Option<S::Params>,
    }

    impl<'a, S: SemaphoreState + ?Sized, C: IsCloseable> Acquire<'a, S, C> {
        fn project<'__pin>(self: Pin<&'__pin mut Self>) -> Projection<'__pin, 'a, S, C> {
            // Safety: This is manual pin-projection. We only need `node` to be `Pin`ned.
            unsafe {
                let Self { sem, node, params } = self.get_unchecked_mut();
                Projection {
                    node: Pin::new_unchecked(node),
                    sem,
                    params,
                }
            }
        }
    }

    impl<S: SemaphoreState + ?Sized, C: IsCloseable> Drop for Acquire<'_, S, C> {
        fn drop(&mut self) {
            // Safety: the value cannot move after the drop phase
            // and we are by definition dropping the value before releasing the memory.
            let pinned_self: Pin<&mut Self> = unsafe { Pin::new_unchecked(self) };
            Self::pinned_drop(pinned_self);
        }
    }
};

impl<S: SemaphoreState + ?Sized, C: IsCloseable> Future for Acquire<'_, S, C> {
    type Output = Result<S::Permit, C::Closed<S::Params>>;

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
                        // unreachable?
                        Err(closed) => return Poll::Ready(Err(C::from_closed(closed, params))),
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

        let queue = match &mut state.queue {
            Ok(queue) => queue,
            Err(_closed) => {
                // Safety: If there is no queue, then we are guaranteed to be removed from it
                let (permit, ()) = unsafe { init.take_removed_unchecked() };
                return Poll::Ready(permit);
            }
        };

        match init.protected_mut(queue) {
            // spurious wakeup
            Some((_, waker)) => {
                waker.clone_from(cx.waker());
                Poll::Pending
            }
            None => {
                // Safety: we have just verified that it is removed;
                let (permit, ()) = unsafe { init.take_removed_unchecked() };
                Poll::Ready(permit)
            }
        }
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
            Err(closed) => return Err(TryAcquireError::Closed(C::from_closed(closed, params))),
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
#[derive(Debug, PartialEq, Eq)]
pub enum TryAcquireError<P, C: IsCloseable> {
    /// The semaphore had no permits to give out right now.
    NoPermits(P),
    /// The semaphore is closed.
    Closed(C::Closed<P>),
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
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq)]
pub struct AcquireError<P> {
    /// The params that was used in the acquire request
    pub params: P,
}

impl<P> fmt::Display for AcquireError<P> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "semaphore closed")
    }
}
