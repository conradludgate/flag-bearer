use std::{
    pin::{Pin, pin},
    task::{Context, Poll},
};

use pin_list::{Node, NodeData};

use crate::{Permit, PinQueue, Semaphore, SemaphoreState};

impl<State: SemaphoreState> Semaphore<State> {
    pub async fn acquire(&self, params: State::Params) -> Permit<'_, State> {
        pin!(Acquire {
            sem: self,
            node: Node::new(),
            params: Some(params),
        })
        .await
    }
}
pin_project_lite::pin_project! {
    struct Acquire<'a, State: SemaphoreState> {
        sem: &'a Semaphore<State>,
        #[pin]
        node: Node<PinQueue<State>>,
        params: Option<State::Params>,
    }

    impl<'a, State: SemaphoreState> PinnedDrop for Acquire<'a, State> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            let Some(node) = this.node.initialized_mut() else { return;};

            let mut state = this.sem.state.lock();
            let (data, _unprotected) = node.reset(&mut state.queue);

            // we were woken, but dropped at the same time. release the permit
            if let NodeData::Removed(permit) = data {
                state.inner.release(permit);
            }

            // if we were the leader, we might have stopped some other task
            // from progressing. check if a new task is ready.
            state.check(this.sem.fifo);
        }
    }
}

impl<'a, State: SemaphoreState> Future for Acquire<'a, State> {
    type Output = Permit<'a, State>;

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
                match state.inner.acquire(params) {
                    Ok(permit) => return Poll::Ready(Permit::new(this.sem, permit)),
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
                Poll::Ready(Permit::new(this.sem, permit))
            }
        }
    }
}
