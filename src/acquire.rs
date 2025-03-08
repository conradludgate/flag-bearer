use core::{
    pin::{Pin, pin},
    task::{Context, Poll},
};

use alloc::sync::Arc;

use pin_list::{Node, NodeData};

use crate::{Permit, PermitOwned, PinQueue, QueueState, Semaphore, SemaphoreState};

pub enum TryAcquireError<S: SemaphoreState> {
    NoPermits(S::Params),
    Closed(S::Params),
}

impl<S: SemaphoreState> Semaphore<S> {
    #[inline]
    fn acquire_inner(&self, params: S::Params) -> impl Future<Output = S::Permit> {
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
    ) -> Result<S::Permit, TryAcquireError<S>> {
        let mut state = self.state.lock();
        match state.unlinked_try_acquire(params, self.fifo, unfair) {
            Ok(permit) => Ok(permit),
            Err(params) => Err(TryAcquireError::NoPermits(params)),
        }
    }

    pub async fn acquire(&self, params: S::Params) -> Permit<'_, S> {
        let permit = self.acquire_inner(params).await;
        Permit::out_of_thin_air(self, permit)
    }

    pub fn try_acquire(&self, params: S::Params) -> Result<Permit<'_, S>, TryAcquireError<S>> {
        let permit = self.try_acquire_inner(params, false)?;
        Ok(Permit::out_of_thin_air(self, permit))
    }

    pub fn try_acquire_unfair(
        &self,
        params: S::Params,
    ) -> Result<Permit<'_, S>, TryAcquireError<S>> {
        let permit = self.try_acquire_inner(params, true)?;
        Ok(Permit::out_of_thin_air(self, permit))
    }

    pub async fn acquire_owned(self: Arc<Self>, params: S::Params) -> PermitOwned<S> {
        let permit = self.acquire_inner(params).await;
        PermitOwned::out_of_thin_air(self, permit)
    }

    pub fn try_acquire_owned(
        self: Arc<Self>,
        params: S::Params,
    ) -> Result<PermitOwned<S>, TryAcquireError<S>> {
        let permit = self.try_acquire_inner(params, false)?;
        Ok(PermitOwned::out_of_thin_air(self, permit))
    }

    pub fn try_acquire_unfair_owned(
        self: Arc<Self>,
        params: S::Params,
    ) -> Result<PermitOwned<S>, TryAcquireError<S>> {
        let permit = self.try_acquire_inner(params, true)?;
        Ok(PermitOwned::out_of_thin_air(self, permit))
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
            if let NodeData::Removed(permit) = data {
                state.state.release(permit);
            }

            // if we were the leader, we might have stopped some other task
            // from progressing. check if a new task is ready.
            state.check(this.sem.fifo);
        }
    }
}

impl<S: SemaphoreState> Future for Acquire<'_, S> {
    type Output = S::Permit;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let mut state = this.sem.state.lock();

        let Some(init) = this.node.as_mut().initialized_mut() else {
            // first time polling.
            let params = this.params.take().unwrap();
            let node = this.node.as_mut();

            match state.unlinked_try_acquire(params, this.sem.fifo, false) {
                Ok(permit) => return Poll::Ready(permit),
                Err(params) => {
                    // no permit or we are not the leader, so we register into the queue.
                    let waker = cx.waker().clone();
                    state.queue.push_back(node, (Some(params), waker), ());
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
