//! Property tests built on the `futures-testing` framework.
//!
//! `futures-testing` drives a single leaf future through an arbitrary sequence
//! of polls and "drive" actions, asserting that whenever the future returns
//! `Pending` it has *registered* its waker (rather than silently dropping it).
//! It polls on arbitrary `Poll` choices, independently of wakeups, so it
//! verifies waker-registration discipline — not that a registered waker is
//! eventually woken. (The latter is covered by the deterministic
//! `fifo_cancel_head_of_line_blocker` test in `semaphore.rs`.)
//!
//! The basic `add_permits` driver is already fuzzed in `semaphore.rs` and
//! `semaphore_lifo.rs`. These cases additionally drive the `close()` and
//! remove-permits code paths, where the waiter is woken (or made permanently
//! unsatisfiable) while it is parked in the queue.

use std::sync::Arc;

use arbitrary::Arbitrary;
use flag_bearer::{Builder, Closeable, Semaphore, SemaphoreState};
use futures_testing::{Driver, TestCase, drive_fn};

#[derive(Debug)]
struct Counter(usize);

impl SemaphoreState for Counter {
    type Params = usize;
    type Permit = usize;

    fn acquire(&mut self, params: usize) -> Result<usize, usize> {
        if params <= self.0 {
            self.0 -= params;
            Ok(params)
        } else {
            Err(params)
        }
    }

    fn release(&mut self, permit: usize) {
        self.0 = self.0.saturating_add(permit);
    }
}

/// Drive actions for the closeable semaphore, chosen arbitrarily by the fuzzer.
#[derive(Arbitrary, Debug)]
enum Op {
    /// Add permits — may wake the waiter with a permit.
    AddPermits(usize),
    /// Remove permits — the waiter must stay parked, keeping its waker.
    RemovePermits(usize),
    /// Close the semaphore — must wake the waiter with an error.
    Close,
}

/// Fuzzes `acquire` against a driver that adds permits, removes permits, and
/// (once) closes the semaphore, in an arbitrary order.
struct CloseableTestCase {
    lifo: bool,
}

impl<'b> TestCase<'b> for CloseableTestCase {
    /// The number of permits the future-under-test requests.
    type Args = usize;

    fn init<'a>(&self, args: &'a mut usize) -> (impl Driver<'b>, impl Future) {
        let builder = if self.lifo {
            Builder::lifo()
        } else {
            Builder::fifo()
        };
        let semaphore: Arc<Semaphore<Counter, Closeable>> =
            Arc::new(builder.closeable().with_state(Counter(0)));

        let driver_sem = semaphore.clone();
        let mut closed = false;
        let driver = drive_fn(move |op: Op| {
            // Once closed, further state changes are meaningless.
            if closed {
                return;
            }
            match op {
                Op::AddPermits(n) => driver_sem.with_state(|s| s.0 = s.0.saturating_add(n)),
                Op::RemovePermits(n) => driver_sem.with_state(|s| s.0 = s.0.saturating_sub(n)),
                Op::Close => {
                    closed = true;
                    driver_sem.close();
                }
            }
        });

        let n = *args;
        let future = async move {
            // Either outcome (a granted permit, or a "closed" error) completes
            // the future; both are acceptable.
            let _ = semaphore.acquire(n).await;
        };

        (driver, future)
    }
}

#[test]
fn closeable_fifo() {
    futures_testing::tests(CloseableTestCase { lifo: false }).run();
}

#[test]
fn closeable_lifo() {
    futures_testing::tests(CloseableTestCase { lifo: true }).run();
}
