//! Limiting two quantities at once — concurrent requests *and* total in-flight
//! bytes — from a single semaphore.
//!
//! Stacking two `tokio` semaphores risks deadlock: you can hold one while
//! waiting for the other. Folding both limits into one `SemaphoreState` makes
//! each acquisition all-or-nothing, and the fair queue orders waiters by a
//! single position.
//!
//! Run with: `cargo run --example multi_dimensional`

use std::sync::Arc;
use std::time::Duration;

use flag_bearer::{Builder, SemaphoreState};

struct Limits {
    requests: usize,
    bytes: u64,
}

/// What a single request needs to reserve. It's both the acquire `Params` and
/// the `Permit`, so dropping the permit returns exactly what was reserved.
struct Reservation {
    bytes: u64,
}

impl SemaphoreState for Limits {
    type Params = Reservation;
    type Permit = Reservation;

    fn acquire(&mut self, req: Reservation) -> Result<Reservation, Reservation> {
        if self.requests >= 1 && self.bytes >= req.bytes {
            self.requests -= 1;
            self.bytes -= req.bytes;
            Ok(req)
        } else {
            Err(req)
        }
    }

    fn release(&mut self, req: Reservation) {
        self.requests += 1;
        self.bytes += req.bytes;
    }
}

#[tokio::main]
async fn main() {
    // Up to 4 concurrent requests, and 1 MiB of in-flight body bytes.
    let sem = Arc::new(Builder::fifo().with_state(Limits {
        requests: 4,
        bytes: 1024 * 1024,
    }));

    // These total well over both budgets, so both dimensions throttle: the
    // count caps at 4 concurrent, and the 800 KiB request can't start until two
    // 512 KiB ones free their bytes.
    let sizes_kib = [512u64, 512, 256, 128, 64, 800];

    let mut handles = Vec::new();
    for (id, kib) in sizes_kib.into_iter().enumerate() {
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.must_acquire(Reservation { bytes: kib * 1024 }).await;
            sem.with_state(|s| {
                println!(
                    "request {id} ({kib:>3} KiB) admitted — {} req / {} KiB still free",
                    s.requests,
                    s.bytes / 1024,
                );
            });
            tokio::time::sleep(Duration::from_millis(100)).await;
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
    println!("all requests served");
}
