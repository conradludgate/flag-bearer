use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, JoinHandle},
    time::Duration,
};

use flag_bearer_core::SemaphoreState;
use flag_bearer_mutex::RawMutex;
use flag_bearer_queue::{FairOrder, SemaphoreQueue, closeable::Closeable};

struct Counter(usize);
impl SemaphoreState for Counter {
    type Params = usize;
    type Permit = usize;
    fn acquire(&mut self, count: usize) -> Result<usize, usize> {
        if count > self.0 {
            return Err(count);
        }
        self.0 -= count;
        Ok(count)
    }
    fn release(&mut self, count: usize) {
        self.0 += count;
    }
}

type Queue = SemaphoreQueue<RawMutex, Counter, Closeable>;
const TIMEOUT: Duration = Duration::from_secs(5);

struct Reenter {
    start: mpsc::Sender<()>,
    done: Mutex<mpsc::Receiver<()>>,
}
impl Wake for Reenter {
    fn wake(self: Arc<Self>) {
        self.start.send(()).unwrap();
        self.done.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
    }
}

fn reentrant_waker(action: impl FnOnce() + Send + 'static) -> (Waker, JoinHandle<()>) {
    let (start, run) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    let helper = thread::spawn(move || {
        run.recv().unwrap();
        action();
        finished.send(()).unwrap();
    });
    (
        Waker::from(Arc::new(Reenter {
            start,
            done: Mutex::new(done),
        })),
        helper,
    )
}

fn finish(operation: impl FnOnce(), helper: JoinHandle<()>) {
    let result = catch_unwind(AssertUnwindSafe(operation));
    // A regression times out in the callback, releasing the outer queue lock
    // during unwinding. Join the helper before failing instead of hanging CI.
    helper.join().unwrap();
    assert!(result.is_ok(), "wake callback could not reenter the queue");
}

#[test]
fn grant_wake_can_acquire_and_release() {
    let queue = Arc::new(Queue::new(Counter(0), FairOrder::Fifo));
    let reentry = Arc::clone(&queue);
    let (waker, helper) = reentrant_waker(move || {
        let Ok(permit) = reentry.try_acquire(1, false) else {
            panic!("a permit remains for the reentrant acquire");
        };
        reentry.with_state(|state| state.release(permit));
    });
    let mut first = Box::pin(queue.acquire(1));
    let mut next = Box::pin(queue.acquire(1));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert!(
        next.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    // The whole queued batch reserves its permits before the first wake. Keep
    // an additional permit available for the callback's separate acquire.
    finish(|| queue.with_state(|state| state.0 = 3), helper);
    assert!(matches!(
        first.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(1))
    ));
    assert!(matches!(
        next.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(1))
    ));
    queue.with_state(|state| state.release(2));
    assert_eq!(queue.with_state(|state| state.0), 3);
}

#[test]
fn close_wake_can_close_again_and_reject_new_waiters() {
    let queue = Arc::new(Queue::new(Counter(0), FairOrder::Fifo));
    let reentry = Arc::clone(&queue);
    let (waker, helper) = reentrant_waker(move || {
        assert!(reentry.is_closed());
        for _ in 0..100 {
            let mut new = Box::pin(reentry.acquire(1));
            assert!(matches!(
                new.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Err(_))
            ));
            assert!(reentry.try_acquire(1, false).is_err());
        }
        reentry.close();
    });
    let mut first = Box::pin(queue.acquire(1));
    let mut rest = (0..32)
        .map(|_| Box::pin(queue.acquire(1)))
        .collect::<Vec<_>>();
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    for next in &mut rest {
        assert!(
            next.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    finish(|| queue.close(), helper);
    assert!(matches!(
        first.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(_))
    ));
    for next in &mut rest {
        assert!(matches!(
            next.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(_))
        ));
    }
    assert!(queue.is_closed());
}

#[test]
fn cancelled_unclaimed_grant_wakes_without_the_mutex() {
    let queue = Arc::new(Queue::new(Counter(0), FairOrder::Fifo));
    let reentry = Arc::clone(&queue);
    let (waker, helper) = reentrant_waker(move || assert!(!reentry.is_closed()));
    let mut first = Box::pin(queue.acquire(1));
    let mut next = Box::pin(queue.acquire(1));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(
        next.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    queue.with_state(|state| state.0 = 1);
    finish(|| drop(first), helper);
    assert!(matches!(
        next.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(1))
    ));
    queue.with_state(|state| state.release(1));
    assert_eq!(queue.with_state(|state| state.0), 1);
}

fn wake_can_cancel_other_waiter(closing: bool) {
    let queue = Arc::new(Queue::new(Counter(0), FairOrder::Fifo));
    let (start, run) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    let (registered, ready) = mpsc::channel();
    let waker = Waker::from(Arc::new(Reenter {
        start,
        done: Mutex::new(done),
    }));
    let mut first = Box::pin(queue.acquire(1));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let mut padding = Vec::new();
    if closing {
        for _ in 0..64 {
            let mut waiter = Box::pin(queue.acquire(1));
            assert!(
                waiter
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            padding.push(waiter);
        }
    }
    let reentry = Arc::clone(&queue);
    let helper = thread::spawn(move || {
        let mut cancelled = Box::pin(reentry.acquire(2));
        assert!(
            cancelled
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        registered.send(()).unwrap();
        run.recv().unwrap();
        drop(cancelled);
        finished.send(()).unwrap();
    });
    ready.recv_timeout(TIMEOUT).unwrap();
    let mut last = Box::pin(queue.acquire(1));
    assert!(
        last.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    finish(
        || {
            if closing {
                queue.close()
            } else {
                queue.with_state(|state| state.0 = 2)
            }
        },
        helper,
    );
    let mut cx = Context::from_waker(Waker::noop());
    if closing {
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
        assert!(matches!(last.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
    } else {
        assert!(matches!(first.as_mut().poll(&mut cx), Poll::Ready(Ok(1))));
        assert!(matches!(last.as_mut().poll(&mut cx), Poll::Ready(Ok(1))));
        queue.with_state(|state| state.release(2));
        assert_eq!(queue.with_state(|state| state.0), 2);
    }
}

#[test]
fn granting_wake_can_cancel_a_queued_head() {
    wake_can_cancel_other_waiter(false);
}

#[test]
fn closing_wake_can_cancel_a_queued_waiter() {
    wake_can_cancel_other_waiter(true);
}

struct CountWake(Arc<AtomicUsize>);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

struct PanicWake;

impl Wake for PanicWake {
    fn wake(self: Arc<Self>) {
        panic!("wake panicked");
    }
}

fn wake_panic_notifies_the_rest_of_the_batch(closing: bool) {
    let queue = Queue::new(Counter(0), FairOrder::Fifo);
    let count = Arc::new(AtomicUsize::new(0));
    let counted = Waker::from(Arc::new(CountWake(Arc::clone(&count))));
    let panicking = Waker::from(Arc::new(PanicWake));
    let mut waiters: Vec<_> = (0..70).map(|_| Box::pin(queue.acquire(1))).collect();
    for (index, waiter) in waiters.iter_mut().enumerate() {
        let waker = if index == 0 { &panicking } else { &counted };
        assert!(
            waiter
                .as_mut()
                .poll(&mut Context::from_waker(waker))
                .is_pending()
        );
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        if closing {
            queue.close();
        } else {
            queue.with_state(|state| state.0 = 70);
        }
    }));
    assert!(result.is_err());
    // The first wake panics, but all other removed waiters must still be
    // notified. Waiters outside that batch remain linked until the retry.
    let notified = count.load(Ordering::Relaxed);
    assert!(notified > 0 && notified < 69);
    assert!(!queue.is_poisoned());
    if closing {
        assert!(queue.is_closed());
        queue.close();
    } else {
        queue.with_state(|_| {});
    }
    assert_eq!(count.load(Ordering::Relaxed), 69);
    let mut cx = Context::from_waker(Waker::noop());
    for waiter in &mut waiters {
        if closing {
            assert!(matches!(waiter.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
        } else {
            assert!(matches!(waiter.as_mut().poll(&mut cx), Poll::Ready(Ok(1))));
        }
    }
    if !closing {
        queue.with_state(|state| state.release(70));
        assert_eq!(queue.with_state(|state| state.0), 70);
    }
}

#[test]
fn grant_wake_panic_notifies_the_rest_of_the_batch() {
    wake_panic_notifies_the_rest_of_the_batch(false);
}

#[test]
fn close_wake_panic_notifies_the_rest_of_the_batch() {
    wake_panic_notifies_the_rest_of_the_batch(true);
}

#[test]
fn acquire_panic_wakes_already_granted_waiters_after_unlocking() {
    struct PanicState(Counter);
    impl SemaphoreState for PanicState {
        type Params = usize;
        type Permit = usize;
        fn acquire(&mut self, count: usize) -> Result<usize, usize> {
            assert!(count != 2 || self.0.0 == 0, "acquire panicked");
            self.0.acquire(count)
        }
        fn release(&mut self, count: usize) {
            self.0.release(count);
        }
    }
    let queue = Arc::new(SemaphoreQueue::<RawMutex, _, Closeable>::new(
        PanicState(Counter(0)),
        FairOrder::Fifo,
    ));
    let reentry = Arc::clone(&queue);
    let (waker, helper) = reentrant_waker(move || assert!(reentry.is_poisoned()));
    let mut first = Box::pin(queue.acquire(1));
    let mut panicking = Box::pin(queue.acquire(2));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert!(
        panicking
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let result = catch_unwind(AssertUnwindSafe(|| queue.with_state(|state| state.0.0 = 3)));
    helper.join().unwrap();
    assert!(result.is_err());
    assert!(queue.is_poisoned());
    assert!(matches!(
        first.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(1))
    ));
    drop(panicking);
    queue.clear_poison();
    queue.with_state(|state| state.release(1));
    assert_eq!(queue.with_state(|state| state.0.0), 3);
}
