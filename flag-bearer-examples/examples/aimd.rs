//! An adaptive concurrency limiter using AIMD (additive-increase /
//! multiplicative-decrease — the same control law as TCP congestion control).
//!
//! The limit grows by a fixed step on each success and is cut by a factor on
//! each failure, so it settles just below where the downstream starts to
//! overload. Because the semaphore's state is data we own, the control loop
//! lives right next to the permit accounting — there's no second structure to
//! keep in sync.
//!
//! Run with: `cargo run --example aimd`

use std::sync::Arc;
use std::time::Duration;

use flag_bearer::{Builder, SemaphoreState};

struct Aimd {
    in_flight: u32,
    limit: f64,
    min: f64,
    max: f64,
}

impl SemaphoreState for Aimd {
    type Params = ();
    type Permit = ();

    fn acquire(&mut self, (): ()) -> Result<(), ()> {
        if f64::from(self.in_flight) < self.limit {
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

impl Aimd {
    fn on_success(&mut self) {
        self.limit = (self.limit + 1.0).min(self.max);
    }

    fn on_failure(&mut self) {
        self.limit = (self.limit * 0.5).max(self.min);
    }

    fn limit(&self) -> u32 {
        self.limit as u32
    }
}

/// A pretend downstream call: fast and reliable at modest concurrency, but it
/// starts failing (overloaded) once more than 8 run at once.
async fn downstream_call(concurrency: u32) -> Result<(), ()> {
    tokio::time::sleep(Duration::from_millis(20)).await;
    if concurrency <= 8 { Ok(()) } else { Err(()) }
}

#[tokio::main]
async fn main() {
    let sem = Arc::new(Builder::fifo().with_state(Aimd {
        in_flight: 0,
        limit: 4.0,
        min: 1.0,
        max: 50.0,
    }));

    let mut handles = Vec::new();
    for n in 1..=200 {
        let task_sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let permit = task_sem.must_acquire(()).await;
            // Sample the concurrency we're entering at, call downstream, then
            // feed the result back into the limit.
            let concurrency = task_sem.with_state(|s| s.in_flight);
            let result = downstream_call(concurrency).await;
            task_sem.with_state(|s| match result {
                Ok(()) => s.on_success(),
                Err(()) => s.on_failure(),
            });
            drop(permit);
        }));

        // Stagger admission a little and report how the limit is tracking.
        if n % 40 == 0 {
            println!(
                "after {n:>3} requests: limit ~= {}",
                sem.with_state(|s| s.limit())
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    for h in handles {
        h.await.unwrap();
    }
    println!(
        "settled near the overload threshold: limit ~= {}",
        sem.with_state(|s| s.limit()),
    );
}
