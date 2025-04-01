use std::sync::Arc;

use flag_bearer::{Semaphore, SemaphoreState};

#[derive(Debug)]
struct Dummy;

impl SemaphoreState for Dummy {
    type Params = ();
    type Permit = ();

    fn acquire(&mut self, _params: Self::Params) -> Result<Self::Permit, Self::Params> {
        Ok(())
    }

    fn release(&mut self, _permit: Self::Permit) {}
}

#[derive(Debug)]
struct Counter(u64);

impl SemaphoreState for Counter {
    type Params = ();
    type Permit = ();

    fn acquire(&mut self, p: Self::Params) -> Result<Self::Permit, Self::Params> {
        let Some(n) = self.0.checked_sub(1) else {
            return Err(p);
        };
        self.0 = n;
        Ok(p)
    }

    fn release(&mut self, _: Self::Permit) {
        self.0 = self.0.saturating_add(1);
    }
}

#[tokio::test]
async fn trait_object() {
    let s1: Arc<Semaphore<dyn SemaphoreState<Params = (), Permit = ()>>> =
        Arc::new(Semaphore::new_fifo(Dummy));

    let s2: Arc<Semaphore<dyn SemaphoreState<Params = (), Permit = ()>>> =
        Arc::new(Semaphore::new_fifo(Counter(1)));

    let _p1 = s1.acquire(()).await.unwrap();
    let _p2 = s2.acquire(()).await.unwrap();
}
