use flag_bearer::{Builder, Order, SemaphoreState};
use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

struct Counter {
    available: usize,
    release_panics: bool,
}

impl SemaphoreState for Counter {
    type Params = ();
    type Permit = ();

    fn acquire(&mut self, (): ()) -> Result<(), ()> {
        if self.available == 0 {
            Err(())
        } else {
            self.available -= 1;
            Ok(())
        }
    }

    fn release(&mut self, (): ()) {
        self.available += 1;
        assert!(
            !self.release_panics,
            "release panicked after mutating state"
        );
    }
}

struct ObserveQueue(Arc<AtomicUsize>);

impl Order for ObserveQueue {
    fn enqueue_front(&mut self, queued: usize) -> bool {
        self.0.store(queued, Ordering::Relaxed);
        false
    }
}

struct PanicWake;

impl Wake for PanicWake {
    fn wake(self: Arc<Self>) {
        panic!("wake panicked");
    }
}

fn wake_panic_keeps_queue_length(closing: bool) {
    let observed = Arc::new(AtomicUsize::new(usize::MAX));
    let sem = Builder::with_order(ObserveQueue(observed.clone()))
        .closeable()
        .with_state(Counter {
            available: 0,
            release_panics: false,
        });
    let waker = Waker::from(Arc::new(PanicWake));
    let mut panic_cx = Context::from_waker(&waker);
    let mut cx = Context::from_waker(Waker::noop());
    let mut first = Box::pin(sem.acquire(()));
    let mut second = Box::pin(sem.acquire(()));
    assert!(first.as_mut().poll(&mut panic_cx).is_pending());
    assert!(second.as_mut().poll(&mut cx).is_pending());

    let result = catch_unwind(AssertUnwindSafe(|| {
        if closing {
            sem.close();
        } else {
            sem.with_state(|state| state.available = 1);
        }
    }));
    assert!(result.is_err());

    if closing {
        // Close is committed before notifications. A panicking waker cannot
        // reopen admission, and polling/retrying close still drains all nodes.
        assert!(sem.is_closed());
        let mut third = Box::pin(sem.acquire(()));
        assert!(matches!(third.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
        assert!(matches!(second.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
        drop(first);
        sem.close();
        assert_eq!(sem.with_state(|state| state.available), 0);
        return;
    }

    // One node has been removed before wake() unwinds. Only `second` remains.
    let mut third = Box::pin(sem.acquire(()));
    assert!(third.as_mut().poll(&mut cx).is_pending());
    assert_eq!(observed.load(Ordering::Relaxed), 1);

    // The first node is removed, so it can be dropped without changing the
    // queue length. Remaining waiters can still receive and return permits.
    drop(first);
    sem.with_state(|state| state.available += if closing { 2 } else { 1 });
    let Poll::Ready(Ok(second)) = second.as_mut().poll(&mut cx) else {
        panic!("second waiter should receive a permit");
    };
    let Poll::Ready(Ok(third)) = third.as_mut().poll(&mut cx) else {
        panic!("third waiter should receive a permit");
    };
    drop((second, third));
    assert_eq!(sem.with_state(|state| state.available), 2);
}

#[test]
fn granting_wake_panic_keeps_queue_length() {
    wake_panic_keeps_queue_length(false);
}

#[test]
fn closing_wake_panic_keeps_queue_closed() {
    wake_panic_keeps_queue_length(true);
}

#[test]
fn cancelled_grant_release_panic_poisons() {
    for closed in [false, true] {
        let sem = Builder::fifo().closeable().with_state(Counter {
            available: 0,
            release_panics: true,
        });
        let mut cx = Context::from_waker(Waker::noop());
        let mut future = Box::pin(sem.acquire(()));
        assert!(future.as_mut().poll(&mut cx).is_pending());
        sem.with_state(|state| state.available = 1);
        if closed {
            sem.close();
        }
        let result = catch_unwind(AssertUnwindSafe(|| drop(future)));
        assert!(result.is_err());
        assert!(
            sem.is_poisoned(),
            "release may have left the state half-updated"
        );
        assert_eq!(sem.is_closed(), closed);
    }
}
