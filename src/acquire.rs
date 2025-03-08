use std::{
    pin::{Pin, pin},
    task::{Context, Poll},
};

use pin_list::{Node, NodeData};

use crate::{Permit, PinQueue, Semaphore, SemaphoreState};

pub enum TryAcquireError<S: SemaphoreState> {
    NoPermits(S::Params),
    Closed(S::Params),
}

impl<S: SemaphoreState> Semaphore<S> {
    pub async fn acquire(&self, params: S::Params) -> Permit<'_, S> {
        pin!(Acquire {
            sem: self,
            node: Node::new(),
            params: Some(params),
        })
        .await
    }

    pub fn try_acquire(&self, mut params: S::Params) -> Result<Permit<'_, S>, TryAcquireError<S>> {
        let mut state = self.state.lock();

        // if last-in-first-out, we are the last in and thus the leader.
        // if first-in-first-out, we are only the leader if the queue is empty.
        let is_leader = !self.fifo || state.queue.is_empty();

        // if we are the leader, try and acquire a permit.
        if is_leader {
            match state.state.acquire(params) {
                Ok(permit) => return Ok(Permit::out_of_thin_air(self, permit)),
                Err(p) => params = p,
            }
        }

        Err(TryAcquireError::NoPermits(params))
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

impl<'a, S: SemaphoreState> Future for Acquire<'a, S> {
    type Output = Permit<'a, S>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let mut state = this.sem.state.lock();

        let Some(init) = this.node.as_mut().initialized_mut() else {
            // first time polling.
            let mut params = this.params.take().unwrap();
            let node = this.node.as_mut();

            // if last-in-first-out, we are the last in and thus the leader.
            // if first-in-first-out, we are only the leader if the queue is empty.
            let is_leader = !this.sem.fifo || state.queue.is_empty();

            // if we are the leader, try and acquire a permit.
            if is_leader {
                match state.state.acquire(params) {
                    Ok(permit) => return Poll::Ready(Permit::out_of_thin_air(this.sem, permit)),
                    Err(p) => params = p,
                }
            }

            // no permit or we are not the leader, so we register into the queue.
            let waker = cx.waker().clone();
            state.queue.push_back(node, (Some(params), waker), ());
            return Poll::Pending;
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
                Poll::Ready(Permit::out_of_thin_air(this.sem, permit))
            }
        }
    }
}
