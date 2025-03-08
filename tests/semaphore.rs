use std::{pin::pin, time::Duration};

use flag_bearer::{Semaphore, SemaphoreState};

#[derive(Debug)]
struct SemaphoreCounter(usize);

impl SemaphoreState for SemaphoreCounter {
    /// Number of permits to acquire
    type Params = usize;

    /// Number of permits that have been acquired
    type Permit = usize;

    fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params> {
        if params <= self.0 {
            self.0 -= params;
            Ok(params)
        } else {
            Err(params)
        }
    }

    fn release(&mut self, permit: Self::Permit) {
        self.0 += permit
    }
}

#[tokio::test]
async fn check() {
    let sem = Semaphore::new_fifo(SemaphoreCounter(1));

    let permit = sem.acquire(1).await;
    sem.with_state(|s| assert_eq!(s.0, 0));

    tokio::time::timeout(Duration::from_millis(100), sem.acquire(1))
        .await
        .expect_err("should timeout while waiting for available permits");

    let mut acq = pin!(sem.acquire(2));

    tokio::time::timeout(Duration::from_millis(100), acq.as_mut())
        .await
        .expect_err("should timeout while waiting for available permits");

    drop(permit);
    sem.with_state(|s| assert_eq!(s.0, 1));

    tokio::time::timeout(Duration::from_millis(100), acq.as_mut())
        .await
        .expect_err("should timeout while waiting for available permits");

    sem.with_state(|s| s.0 += 1);

    let permit = acq.await;
    sem.with_state(|s| assert_eq!(s.0, 0));

    drop(permit);
    sem.with_state(|s| assert_eq!(s.0, 2));
}
