//! `Random` is an external [`flag_bearer::Order`] (defined in this crate) that
//! enqueues each waiter at the front with a logistic (sigmoid) probability in the
//! queue length — FIFO below the midpoint, LIFO above. Built via `Builder::with_order`.

use std::task::{Context, Poll};

use flag_bearer::{Builder, Order, SemaphoreState};
use flag_bearer_tests::Random;
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
fn random_sigmoid_leans_by_queue_length() {
    // The logistic curve should keep new arrivals at the back (FIFO) well below the
    // midpoint, and send them to the front (LIFO) well above it.
    let mut order = Random::new(10);
    let trials = 1000;
    let front_when_short = (0..trials).filter(|_| order.enqueue_front(1)).count();
    let front_when_long = (0..trials).filter(|_| order.enqueue_front(100)).count();
    assert!(
        front_when_short < trials / 5,
        "queue well below midpoint should be mostly FIFO; got {front_when_short}/{trials} front",
    );
    assert!(
        front_when_long > trials * 4 / 5,
        "queue well above midpoint should be mostly LIFO; got {front_when_long}/{trials} front",
    );
}

#[test]
fn random_serves_all_waiters() {
    // bias > 0 actually draws from the RNG on each enqueue. Whatever sides it picks,
    // the queue stays consistent and every waiter is served once permits exist.
    let s = Builder::with_order(Random::new(4)).with_state(Counter(0));
    let mut cx = Context::from_waker(noop_waker_ref());

    let mut waiters: Vec<_> = (0..20).map(|_| Box::pin(s.acquire(1))).collect();
    for w in &mut waiters {
        assert!(w.as_mut().poll(&mut cx).is_pending());
    }

    // One permit per waiter; check() drains the whole (randomly-ordered) queue.
    s.with_state(|c| c.0 += 20);

    let mut permits = Vec::new();
    for w in &mut waiters {
        let Poll::Ready(Ok(p)) = w.as_mut().poll(&mut cx) else {
            panic!("every waiter should be served once enough permits exist");
        };
        permits.push(p);
    }
    assert_eq!(permits.len(), 20);
}
