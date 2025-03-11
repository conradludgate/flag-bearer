use std::{
    hint::black_box,
    sync::Barrier,
    thread::available_parallelism,
    time::{Duration, Instant},
};

use hdrhistogram::Histogram;

fn main() {
    let t = available_parallelism().unwrap().get();

    let rounds = 50000;
    let iters = 50;

    println!("flag_bearer[threads = {t}]:");
    let semaphore = semaphore::Semaphore::new(0);
    let h = bench(t, rounds, iters, &semaphore, async |s| {
        black_box(s.try_acquire());
    });
    print_results(h);

    println!("tokio[threads = {t}]:");
    let semaphore = tokio::sync::Semaphore::new(0);
    let h = bench(t, rounds, iters, &semaphore, async |s| {
        drop(black_box(s.try_acquire()));
    });
    print_results(h);
}

fn print_results(h: Histogram<u64>) {
    for q in [0.5, 0.75, 0.9, 0.99] {
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
            Self(flag_bearer::Semaphore::new_fifo(SemaphoreCounter(count)))
        }

        pub fn try_acquire(&self) -> Option<Permit<'_>> {
            Some(Permit(self.0.try_acquire(1).ok()?))
        }
    }
}
