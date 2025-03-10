use core::{
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use pin_list::{Node, NodeData};

use crate::{Permit, PinQueue, QueueState, Semaphore, SemaphoreState};

impl<S: SemaphoreState> Semaphore<S> {
    #[inline]
    fn acquire_inner(
        &self,
        params: S::Params,
    ) -> impl Future<Output = Result<S::Permit, S::Params>> {
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
        unfair: bool,
    ) -> Result<S::Permit, TryAcquireError<S::Params>> {
        let mut state = self.state.lock();
        match state.unlinked_try_acquire(params, self.fifo, unfair) {
            Ok(permit) => Ok(permit),
            Err(params) => Err(TryAcquireError::NoPermits(params)),
        }
    }

    /// Acquire a new permit fairly with the given parameters.
    ///
    /// If a permit is not immediately available, this task will
    /// join a queue.
    pub async fn acquire(
        &self,
        params: S::Params,
    ) -> Result<Permit<'_, S>, AcquireError<S::Params>> {
        let permit = self
            .acquire_inner(params)
            .await
            .map_err(|params| AcquireError { params })?;
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
    ) -> Result<Permit<'_, S>, TryAcquireError<S::Params>> {
        let permit = self.try_acquire_inner(params, false)?;
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
    ) -> Result<Permit<'_, S>, TryAcquireError<S::Params>> {
        let permit = self.try_acquire_inner(params, true)?;
        Ok(Permit::out_of_thin_air(self, permit))
    }
}

pin_project_lite::pin_project! {
    struct Acquire<'a, S: SemaphoreState> {
        sem: &'a Semaphore<S>,
        #[pin]
        node: Node<PinQueue<S>>,
        params: Option<S::Params>,
    }

    impl<'a, S: SemaphoreState> PinnedDrop for Acquire<'a, S> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let Some(node) = this.node.initialized_mut() else { return;};

            let mut state = this.sem.state.lock();
            let (data, _unprotected) = node.reset(&mut state.queue);

            // we were woken, but dropped at the same time. release the permit
            if let NodeData::Removed(Ok(permit)) = data {
                state.state.release(permit);
            }

            // if we were the leader, we might have stopped some other task
            // from progressing. check if a new task is ready.
            state.check();
        }
    }
}

impl<S: SemaphoreState> Future for Acquire<'_, S> {
    type Output = Result<S::Permit, S::Params>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let mut state = this.sem.state.lock();

        let Some(init) = this.node.as_mut().initialized_mut() else {
            // first time polling.
            let params = this.params.take().unwrap();
            let node = this.node.as_mut();

            match state.unlinked_try_acquire(params, this.sem.fifo, false) {
                Ok(permit) => return Poll::Ready(Ok(permit)),
                Err(params) => {
                    // no permit or we are not the leader, so we register into the queue.
                    let waker = cx.waker().clone();
                    if this.sem.fifo {
                        state.queue.push_back(node, (Some(params), waker), ());
                    } else {
                        state.queue.push_front(node, (Some(params), waker), ());
                    }
                    return Poll::Pending;
                }
            }
        };

        match init.protected_mut(&mut state.queue) {
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

impl<S: SemaphoreState> QueueState<S> {
    #[inline]
    fn unlinked_try_acquire(
        &mut self,
        params: S::Params,
        fifo: bool,
        unfair: bool,
    ) -> Result<S::Permit, S::Params> {
        // if last-in-first-out, we are the last in and thus the leader.
        // if first-in-first-out, we are only the leader if the queue is empty.
        let is_leader = unfair || !fifo || self.queue.is_empty();

        // if we are the leader, try and acquire a permit.
        if is_leader {
            match self.state.acquire(params) {
                Ok(permit) => Ok(permit),
                Err(p) => Err(p),
            }
        } else {
            Err(params)
        }
    }
}

/// The error returned by [`try_acquire`](Semaphore::try_acquire)
#[derive(Debug, PartialEq, Eq)]
pub enum TryAcquireError<P> {
    NoPermits(P),
    Closed(P),
}

impl<P> fmt::Display for TryAcquireError<P> {
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
    pub params: P,
}

impl<P> fmt::Display for AcquireError<P> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(fmt, "semaphore closed")
    }
}
