use std::{
    hint::black_box,
    sync::Barrier,
    thread::available_parallelism,
    time::{Duration, Instant},
};

use hdrhistogram::Histogram;

fn main() {
    let t = available_parallelism().unwrap().get();

    let slow = t / 2;
    let fast = t * 2;
    let rounds = 50000;
    let iters = 50;

    println!("flag_bearer[threads = {t}, permits = {slow}]:");
    let semaphore = semaphore::Semaphore::new(slow);
    let h = bench(t, rounds, iters, &semaphore, async |s| {
        black_box(s.acquire().await);
    });
    print_results(h);

    println!("flag_bearer[threads = {t}, permits = {fast}]:");
    let semaphore = semaphore::Semaphore::new(fast);
    let h = bench(t, rounds, iters, &semaphore, async |s| {
        black_box(s.acquire().await);
    });
    print_results(h);

    println!("flag_bearer[threads = 2, permits = 64]:");
    let semaphore = semaphore::Semaphore::new(64);
    let h = bench(2, rounds, iters, &semaphore, async |s| {
        black_box(s.acquire().await);
    });
    print_results(h);

    println!("tokio[threads = {t}, permits = {slow}]:");
    let semaphore = tokio::sync::Semaphore::new(slow);
    let h = bench(t, rounds, iters, &semaphore, async |s| {
        drop(black_box(s.acquire().await.unwrap()));
    });
    print_results(h);

    println!("tokio[threads = {t}, permits = {fast}]:");
    let semaphore = tokio::sync::Semaphore::new(fast);
    let h = bench(t, rounds, iters, &semaphore, async |s| {
        drop(black_box(s.acquire().await.unwrap()));
    });
    print_results(h);

    println!("tokio[threads = 2, permits = 64]:");
    let semaphore = tokio::sync::Semaphore::new(64);
    let h = bench(2, rounds, iters, &semaphore, async |s| {
        drop(black_box(s.acquire().await.unwrap()));
    });
    print_results(h);
}

fn print_results(h: Histogram<u64>) {
    for q in [0.5, 0.75, 0.9, 0.99, 0.999] {
        println!(
            "\t{}'th percentile of data is {:?}",
            q * 100.0,
            Duration::from_nanos(h.value_at_quantile(q)),
        );
    }
    println!()
}

fn bench<S: Sync>(
    threads: usize,
    rounds: usize,
    iterations: u32,
    state: &S,
    f: impl AsyncFn(&S) + Sync,
) -> Histogram<u64> {
    let barrier = Barrier::new(threads);
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let warmup = 100;

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(threads);

        for _ in 0..threads {
            handles.push(s.spawn(|| {
                let mut h = Histogram::<u64>::new(3).unwrap();
                for round in 0..rounds + warmup {
                    if round == warmup {
                        h.reset();
                    }

                    barrier.wait();
                    let start = Instant::now();

                    runtime.block_on(async {
                        for _ in 0..iterations {
                            f(state).await
                        }
                    });

                    let d = start.elapsed() / iterations;
                    h += d.as_nanos() as u64;
                }
                h
            }));
        }

        let mut h = Histogram::<u64>::new(3).unwrap();
        for handle in handles {
            h += handle.join().unwrap();
        }

        h
    })
}

mod semaphore {
    use flag_bearer::Uncloseable;

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
    pub struct Permit<'a>(flag_bearer::Permit<'a, SemaphoreCounter, Uncloseable>);

    impl Semaphore {
        pub fn new(count: usize) -> Self {
            Self(flag_bearer::SemaphoreBuilder::fifo().with_state(SemaphoreCounter(count)))
        }

        pub async fn acquire(&self) -> Permit<'_> {
            Permit(self.0.acquire(1).await.unwrap_or_else(|x| x.never()))
        }
    }
}
