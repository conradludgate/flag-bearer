//! A concurrency limiter whose limit can be raised *and lowered* at runtime —
//! even below the number of permits currently in flight.
//!
//! `tokio::sync::Semaphore` can add permits but can't remove already-issued
//! ones (you'd have to `forget` permits you don't hold). Here the limit is just
//! a field in our state, so one `with_state` call retunes it; acquisitions
//! simply pause until in-flight work drains back under the new ceiling.
//!
//! Run with: `cargo run --example dynamic_limit`

use std::sync::Arc;
use std::time::Duration;

use flag_bearer::{Builder, SemaphoreState};

/// Tracks how many permits are out, and the current ceiling.
struct ConcurrencyLimit {
    in_flight: usize,
    limit: usize,
}

impl SemaphoreState for ConcurrencyLimit {
    type Params = ();
    type Permit = ();

    fn acquire(&mut self, (): ()) -> Result<(), ()> {
        if self.in_flight < self.limit {
            self.in_flight += 1;
            Ok(())
        } else {
            Err(())
        }
    }

    fn release(&mut self, (): ()) {
        self.in_flight -= 1;
    }
}

#[tokio::main]
async fn main() {
    let sem = Arc::new(Builder::fifo().with_state(ConcurrencyLimit {
        in_flight: 0,
        limit: 4,
    }));

    // Spawn 12 workers; each holds a permit for a little while.
    let mut workers = Vec::new();
    for id in 0..12 {
        let sem = sem.clone();
        workers.push(tokio::spawn(async move {
            let _permit = sem.must_acquire(()).await;
            let in_flight = sem.with_state(|s| s.in_flight);
            println!("worker {id:>2} running ({in_flight} in flight)");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }));
    }

    // After a moment, tighten the limit from 4 down to 1. Workers already
    // running keep their permits, but no new worker starts until in-flight
    // work falls back under the new ceiling.
    tokio::time::sleep(Duration::from_millis(50)).await;
    println!("-- shrinking limit to 1 --");
    sem.with_state(|s| s.limit = 1);

    for w in workers {
        w.await.unwrap();
    }
    println!("all workers done");
}
