//! `Builder::hybrid(lifo_above)` enqueues FIFO while the queue is short and LIFO
//! once it grows past the threshold — predictable when calm, LIFO-like under load.

use std::task::{Context, Poll};

use flag_bearer::{Builder, SemaphoreState};
use futures_util::task::noop_waker_ref;

#[derive(Debug)]
struct Counter(usize);

impl SemaphoreState for Counter {
    type Params = usize;
    type Permit = usize;

    fn acquire(&mut self, n: usize) -> Result<usize, usize> {
        if n <= self.0 {
            self.0 -= n;
            Ok(n)
        } else {
            Err(n)
        }
    }

    fn release(&mut self, n: usize) {
        self.0 += n;
    }
}

#[test]
fn hybrid_switches_to_lifo_above_threshold() {
    // FIFO while <= 2 are waiting, LIFO beyond that. Starts with no permits.
    let s = Builder::hybrid(2).with_state(Counter(0));
    let mut cx = Context::from_waker(noop_waker_ref());

    // Four waiters queue up. a1/a2/a3 join while the queue is within the FIFO
    // regime (0, 1, 2 already waiting), so they line up at the back in order.
    // a4 arrives with 3 already waiting (> 2) and jumps to the front (LIFO).
    let mut a1 = Box::pin(s.acquire(1));
    let mut a2 = Box::pin(s.acquire(1));
    let mut a3 = Box::pin(s.acquire(1));
    let mut a4 = Box::pin(s.acquire(1));
    assert!(a1.as_mut().poll(&mut cx).is_pending());
    assert!(a2.as_mut().poll(&mut cx).is_pending());
    assert!(a3.as_mut().poll(&mut cx).is_pending());
    assert!(a4.as_mut().poll(&mut cx).is_pending());

    // Hand out one permit at a time. Expected service order: a4 (jumped), then the
    // FIFO backlog a1, a2, a3. Permits are held (bound) so they don't release back.

    // a4 is served first, ahead of the older waiters.
    s.with_state(|c| c.0 += 1);
    assert!(a1.as_mut().poll(&mut cx).is_pending());
    assert!(a2.as_mut().poll(&mut cx).is_pending());
    assert!(a3.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Ok(_p4)) = a4.as_mut().poll(&mut cx) else {
        panic!("a4 should jump the queue under the LIFO regime");
    };

    // Then the backlog drains in FIFO order: a1, a2, a3.
    s.with_state(|c| c.0 += 1);
    let Poll::Ready(Ok(_p1)) = a1.as_mut().poll(&mut cx) else {
        panic!("a1 should be served next (FIFO backlog)");
    };

    s.with_state(|c| c.0 += 1);
    assert!(a3.as_mut().poll(&mut cx).is_pending(), "a2 comes before a3");
    let Poll::Ready(Ok(_p2)) = a2.as_mut().poll(&mut cx) else {
        panic!("a2 should be served before a3");
    };

    s.with_state(|c| c.0 += 1);
    let Poll::Ready(Ok(_p3)) = a3.as_mut().poll(&mut cx) else {
        panic!("a3 should be served last");
    };
}
