use core::{
    pin::Pin,
    task::{Context, Poll},
};

use lock_api::RawMutex;
use pin_list::{Node, NodeData};

use crate::acquire::{FairOrder, Fairness, TryAcquireError};
use crate::{SemaphoreQueue, SemaphoreState, closeable::Uncloseable};

use super::PinQueue;

use crate::loom::Mutex;

pin_project_lite::pin_project! {
    /// A [`Future`] that acquires a permit from a [`SemaphoreQueue`].
    pub struct AcquireWhile<'a, S, F, R>
    where
        S: ?Sized,
        S: SemaphoreState,
        R: RawMutex,
    {
        #[pin]
        node: Node<PinQueue<S::Params, S::Permit, Uncloseable>>,
        order: FairOrder,
        state: &'a Mutex<R, SemaphoreQueue<S, Uncloseable>>,
        params: Option<S::Params>,
        f: F,
    }

    impl<S, F, R> PinnedDrop for AcquireWhile<'_, S, F, R>
    where
        S: ?Sized,
        S: SemaphoreState,
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
                    let Ok(permit) = permit;
                        state.state.release(permit);
                }
            }
        }
    }
}

impl<S, F, E, R> Future for AcquireWhile<'_, S, F, R>
where
    S: SemaphoreState + ?Sized,
    F: FnMut(&mut S, &S::Params, &mut Context) -> Poll<E>,
    R: RawMutex,
{
    type Output = Result<S::Permit, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let mut state = this.state.lock();

        let Some(init) = this.node.as_mut().initialized_mut() else {
            // first time polling.
            let params = this.params.take().unwrap();
            let node = this.node.as_mut();

            return match state.try_acquire(params, Fairness::Fair(*this.order)) {
                Ok(permit) => Poll::Ready(Ok(permit)),
                Err(TryAcquireError::Closed(params)) => params.never(),
                Err(TryAcquireError::NoPermits(params)) => {
                    if let Poll::Ready(e) = (this.f)(&mut state.state, &params, cx) {
                        return Poll::Ready(Err(e));
                    }

                    let queue = match &mut state.queue {
                        Ok(queue) => queue,
                        Err(x) => match *x {},
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

        let SemaphoreQueue { queue, state } = &mut *state;
        if let Ok(queue) = queue {
            if let Some((params, waker)) = init.protected_mut(queue) {
                let params = params.as_ref().expect(
                    "params should be set. likely the SemaphoreState::acquire method panicked",
                );
                if let Poll::Ready(e) = (this.f)(state, params, cx) {
                    return Poll::Ready(Err(e));
                }

                // spurious wakeup
                waker.clone_from(cx.waker());
                return Poll::Pending;
            }
        }

        // Safety: Either there is no queue, then we are guaranteed to be removed from it
        // Or there was a queue, but we were removed from it anyway (protected_mut returned None).
        let (permit, ()) = unsafe { init.take_removed_unchecked() };
        let permit = permit.map_err(|x| match x {});
        Poll::Ready(permit)
    }
}

impl<S: SemaphoreState + ?Sized> SemaphoreQueue<S, Uncloseable> {
    /// Acquire a permit, or join the queue if not currently available.
    ///
    /// * If the order is [`FairOrder::Lifo`], then we enqueue at the front of the queue.
    /// * If the order is [`FairOrder::Fifo`], then we enqueue at the back of the queue.
    #[inline]
    pub fn acquire_while<F, E, R: RawMutex>(
        this: &Mutex<R, Self>,
        params: S::Params,
        order: FairOrder,
        f: F,
    ) -> AcquireWhile<'_, S, F, R>
    where
        F: FnMut(&mut S, &S::Params, &mut Context) -> Poll<E>,
    {
        AcquireWhile {
            node: Node::new(),
            order,
            state: this,
            params: Some(params),
            f,
        }
    }
}
