pub mod bench;

use flag_bearer::Order;
use rand::{RngExt, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;

/// An *external* [`Order`](flag_bearer::Order) implementation: a randomized
/// FIFO↔LIFO blend that owns its RNG.
///
/// The probability of joining the front follows a **logistic (sigmoid)** curve in
/// the current queue length: ≈0 (FIFO) while well below `midpoint`, 0.5 at the
/// `midpoint`, and ≈1 (LIFO) well above it. `steepness` sets how sharp the
/// transition is — large values approach [`Hybrid`](flag_bearer::Hybrid)'s hard
/// cliff, small values give a gentle ramp.
///
/// It lives here rather than in the published crates so they stay free of a `rand`
/// dependency, and to exercise [`Order`] as a genuine third-party policy.
pub struct Random<R = Xoshiro256PlusPlus> {
    /// Queue length at which P(front) = 0.5.
    pub midpoint: f64,
    /// Logistic steepness (per unit of queue length).
    pub steepness: f64,
    pub rng: R,
}

impl Random {
    /// 50% point at `midpoint`, with a default steepness that spreads the bulk of
    /// the transition over roughly `midpoint/2 ..= 3*midpoint/2`. Fixed seed, so
    /// tests are reproducible.
    pub fn new(midpoint: usize) -> Self {
        let midpoint = midpoint as f64;
        Self {
            midpoint,
            steepness: if midpoint > 0.0 { 4.0 / midpoint } else { 1.0 },
            rng: Xoshiro256PlusPlus::seed_from_u64(0x9E37_79B9_7F4A_7C15),
        }
    }
}

impl<R: rand::Rng> Order for Random<R> {
    fn enqueue_front(&mut self, queued: usize) -> bool {
        // Logistic: P(front) = 1 / (1 + e^{-steepness·(queued - midpoint)}). Always
        // in (0, 1), so `random_bool` can't panic.
        let x = self.steepness * (queued as f64 - self.midpoint);
        let p_front = 1.0 / (1.0 + (-x).exp());
        self.rng.random_bool(p_front)
    }
}

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
        Self(flag_bearer::Builder::fifo().with_state(SemaphoreCounter(count)))
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
