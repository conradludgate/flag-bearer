pub mod bench;

#[derive(Debug)]
struct SemaphoreCounter(usize);

impl flag_bearer::SemaphoreState for SemaphoreCounter {
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
        self.0 = self.0.checked_add(permit).unwrap()
    }
}

pub struct Semaphore(flag_bearer::Semaphore<SemaphoreCounter>);

#[allow(dead_code)]
pub struct Permit<'a>(flag_bearer::Permit<'a, SemaphoreCounter>);

impl Semaphore {
    pub fn new(count: usize) -> Self {
        Self(flag_bearer::new_fifo().with_state(SemaphoreCounter(count)))
    }

    #[allow(dead_code)]
    pub async fn acquire(&self) -> Permit<'_> {
        Permit(self.0.acquire(1).await.unwrap_or_else(|x| x.never()))
    }

    #[allow(dead_code)]
    pub fn try_acquire(&self) -> Option<Permit<'_>> {
        Some(Permit(self.0.try_acquire(1).ok()?))
    }
}
