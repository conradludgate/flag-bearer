use std::{
    hint::black_box,
    sync::Barrier,
    time::{Duration, Instant},
};

use hdrhistogram::Histogram;

#[inline(never)]
pub fn async_bench<S: Sync>(
    threads: usize,
    rounds: usize,
    iterations: u32,
    state: S,
    f: impl AsyncFn(&S) + Sync,
) {
    let h = {
        bench_threads(threads, rounds, &|barrier| {
            pollster::block_on(async {
                barrier.wait();
                let start = Instant::now();

                for _ in 0..iterations {
                    f(black_box(&state)).await
                }

                start.elapsed() / iterations
            })
        })
    };

    for q in [0.5, 0.75, 0.9, 0.99, 0.999] {
        println!(
            "\t{}'th percentile of data is {:?}",
            q * 100.0,
            Duration::from_nanos(h.value_at_quantile(q)),
        );
    }
    println!()
}

#[inline(never)]
fn bench_threads(
    threads: usize,
    rounds: usize,
    f: &(dyn Fn(&Barrier) -> Duration + Sync),
) -> Histogram<u64> {
    let barrier = Barrier::new(threads);
    let warmup = rounds / 100;
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(threads);

        for _ in 0..threads {
            handles.push(s.spawn(|| {
                let mut h = Histogram::<u64>::new(3).unwrap();

                for round in 0..rounds + warmup {
                    if round == warmup {
                        h.reset();
                    }
                    h += f(&barrier).as_nanos() as u64;
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
