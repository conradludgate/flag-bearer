use std::{
    pin::{Pin, pin},
    task::{Context, Poll},
};

use pin_list::{Cursor, Node};

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
        node: Node<PinQueue>,
        params: Option<State::Params>,
    }

    impl<'a, State: SemaphoreState> PinnedDrop for Acquire<'a, State> {
        fn drop(this: Pin<&mut Self>) {
            let this = this.project();
            if let Some(node) = this.node.initialized_mut() {
                node.reset(&mut this.sem.state.lock().queue);
            }
        }
    }
}

impl<'a, State: SemaphoreState> Future for Acquire<'a, State> {
    type Output = Permit<'a, State>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();
        let mut state = this.sem.state.lock();

        let init = if let Some(init) = this.node.as_mut().initialized_mut() {
            init
        } else {
            state
                .queue
                .push_back(this.node.as_mut(), cx.waker().clone(), ())
        };

        let waker = init.protected_mut(&mut state.queue).unwrap();
        waker.clone_from(cx.waker());

        if !is_leader(init.cursor(&state.queue).unwrap(), this.sem.fifo) {
            return Poll::Pending;
        }

        match state.inner.acquire(this.params.take().unwrap()) {
            Err(p) => {
                *this.params = Some(p);
                Poll::Pending
            }
            Ok(permit) => {
                _ = init
                    .cursor_mut(&mut state.queue)
                    .unwrap()
                    .remove_current(())
                    .unwrap();
                if state.inner.permits_available() {
                    let next = if this.sem.fifo {
                        state.queue.cursor_front()
                    } else {
                        state.queue.cursor_back()
                    };
                    if let Some(waker) = next.protected() {
                        waker.wake_by_ref();
                    }
                }

                Poll::Ready(Permit::new(this.sem, permit))
            }
        }
    }
}

fn is_leader(mut cursor: Cursor<'_, PinQueue>, fifo: bool) -> bool {
    if fifo {
        cursor.move_previous();
    } else {
        cursor.move_next();
    }
    cursor.unprotected().is_none()
}
