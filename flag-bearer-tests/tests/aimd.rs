use std::time::Duration;

use flag_bearer::SemaphoreState;

#[derive(Debug)]
pub struct Aimd {
    limit: u64,
    config: AimdConfig,
}

#[derive(Debug)]
pub struct AimdConfig {
    pub min: u64,
    pub max: u64,
    pub inc: u64,
    pub dec: f64,
}

impl Aimd {
    pub fn new(config: AimdConfig) -> Self {
        Self {
            limit: config.max,
            config,
        }
    }

    pub fn success(&mut self) {
        self.limit += self.config.inc;
        self.limit = self.limit.clamp(self.config.min, self.config.max);
    }

    pub fn failure(&mut self) {
        self.limit = ((self.limit as f64) * self.config.dec).round() as u64;
        self.limit = self.limit.clamp(self.config.min, self.config.max);
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }
}

#[derive(Debug)]
struct AimdState {
    /// how many permits are currently taken
    acquired: u64,
    /// the aimd state
    state: Aimd,
}

impl AimdState {
    pub fn available(&self) -> u64 {
        // saturating as acquired can be greater than limit, if we experienced failures.
        self.state.limit().saturating_sub(self.acquired)
    }

    pub fn success(&mut self) {
        self.state.success();
    }

    pub fn failure(&mut self) {
        self.state.failure();
    }
}

impl SemaphoreState for AimdState {
    type Params = ();
    type Permit = ();

    fn acquire(&mut self, _: ()) -> Result<(), ()> {
        if self.acquired < self.state.limit {
            self.acquired += 1;
            Ok(())
        } else {
            Err(())
        }
    }

    fn release(&mut self, _: ()) {
        self.acquired -= 1;
    }
}

#[tokio::test]
async fn check() {
    let config = AimdConfig {
        min: 1,
        max: 10,
        inc: 1,
        dec: 0.5,
    };
    let sem = flag_bearer::new_fifo().with_state(AimdState {
        acquired: 0,
        state: Aimd::new(config),
    });

    assert_eq!(sem.with_state(|s| s.available()), 10);

    let permit1 = sem.acquire(()).await.unwrap_or_else(|x| x.never());
    assert_eq!(sem.with_state(|s| s.available()), 9);

    sem.with_state(|s| s.failure());
    assert_eq!(sem.with_state(|s| s.available()), 4);

    sem.with_state(|s| s.success());
    assert_eq!(sem.with_state(|s| s.available()), 5);

    sem.with_state(|s| s.failure());
    assert_eq!(sem.with_state(|s| s.available()), 2);

    let _permit2 = sem.acquire(()).await.unwrap_or_else(|x| x.never());
    let _permit3 = sem.acquire(()).await.unwrap_or_else(|x| x.never());
    assert_eq!(sem.with_state(|s| s.available()), 0);

    tokio::time::timeout(Duration::from_millis(100), sem.acquire(()))
        .await
        .expect_err("should timeout while waiting for available permits");

    drop(permit1);
    assert_eq!(sem.with_state(|s| s.available()), 1);
}
