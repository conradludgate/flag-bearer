use core::{future::Future, task::Context};
use flag_bearer_core::SemaphoreState;
use flag_bearer_mutex::RawMutex;
use flag_bearer_queue::{FairOrder, SemaphoreQueue, closeable::Closeable};
use std::task::{Poll, Waker};

#[derive(Debug)]
struct Counter(usize);

impl SemaphoreState for Counter {
    type Params = usize;
    type Permit = usize;

    fn acquire(&mut self, count: usize) -> Result<usize, usize> {
        if count <= self.0 {
            self.0 -= count;
            Ok(count)
        } else {
            Err(count)
        }
    }

    fn release(&mut self, count: usize) {
        self.0 += count;
    }
}

fn cancelled_grant_is_returned<R: lock_api::RawMutex>() {
    let queue = SemaphoreQueue::<R, _, Closeable>::new(Counter(0), FairOrder::Fifo);
    let mut cx = Context::from_waker(Waker::noop());
    let mut first = Box::pin(queue.acquire(1));
    let mut second = Box::pin(queue.acquire(1));
    assert!(first.as_mut().poll(&mut cx).is_pending());
    assert!(second.as_mut().poll(&mut cx).is_pending());

    queue.with_state(|state| state.0 = 1);
    drop(first);
    assert!(matches!(second.as_mut().poll(&mut cx), Poll::Ready(Ok(1))));
    queue.with_state(|state| state.release(1));
    assert_eq!(queue.with_state(|state| state.0), 1);
}

#[test]
fn default_mutex_returns_cancelled_grant() {
    cancelled_grant_is_returned::<RawMutex>();
}

#[test]
fn custom_mutex_returns_cancelled_grant() {
    cancelled_grant_is_returned::<parking_lot::RawMutex>();
}

#[test]
fn cancelling_head_services_next_waiter() {
    let queue = SemaphoreQueue::<RawMutex, _, Closeable>::new(Counter(0), FairOrder::Fifo);
    let mut cx = Context::from_waker(Waker::noop());
    let mut head = Box::pin(queue.acquire(2));
    let mut next = Box::pin(queue.acquire(1));
    assert!(head.as_mut().poll(&mut cx).is_pending());
    assert!(next.as_mut().poll(&mut cx).is_pending());

    queue.with_state(|state| state.0 = 1);
    assert!(next.as_mut().poll(&mut cx).is_pending());
    drop(head);
    assert!(matches!(next.as_mut().poll(&mut cx), Poll::Ready(Ok(1))));
    queue.with_state(|state| state.release(1));
    assert_eq!(queue.with_state(|state| state.0), 1);
}

#[test]
fn close_removes_waiters_before_they_are_polled_or_dropped() {
    let queue = SemaphoreQueue::<RawMutex, _, Closeable>::new(Counter(0), FairOrder::Fifo);
    let mut cx = Context::from_waker(Waker::noop());
    let mut polled = Box::pin(queue.acquire(1));
    let mut cancelled = Box::pin(queue.acquire(2));
    assert!(polled.as_mut().poll(&mut cx).is_pending());
    assert!(cancelled.as_mut().poll(&mut cx).is_pending());

    queue.close();
    assert!(queue.is_closed());
    drop(cancelled);
    let Poll::Ready(Err(error)) = polled.as_mut().poll(&mut cx) else {
        panic!("closed waiter should receive an error");
    };
    assert_eq!(error.params, 1);
    queue.close();
    assert!(queue.try_acquire(1, true).is_err());
}

#[test]
fn close_preserves_an_unclaimed_grant_until_cancellation() {
    let queue = SemaphoreQueue::<RawMutex, _, Closeable>::new(Counter(0), FairOrder::Fifo);
    let mut cx = Context::from_waker(Waker::noop());
    let mut acquire = Box::pin(queue.acquire(1));
    assert!(acquire.as_mut().poll(&mut cx).is_pending());
    queue.with_state(|state| state.0 = 1);
    queue.close();
    drop(acquire);
    assert_eq!(queue.with_state(|state| state.0), 1);
}

#[test]
fn dynamic_state_and_locked_debug_remain_supported() {
    let queue = SemaphoreQueue::<RawMutex, _, Closeable>::new(Counter(1), FairOrder::Fifo);
    let erased: &SemaphoreQueue<
        RawMutex,
        dyn SemaphoreState<Params = usize, Permit = usize>,
        Closeable,
    > = &queue;
    assert!(matches!(erased.try_acquire(1, true), Ok(1)));
    erased.with_state(|state| state.release(1));
    assert_eq!(queue.with_state(|_| format!("{queue:?}")), "<locked>");
}
