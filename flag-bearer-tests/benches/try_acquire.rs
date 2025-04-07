use std::{hint::black_box, thread::available_parallelism};

use flag_bearer_tests::bench::async_bench;

fn main() {
    let t = available_parallelism().unwrap().get();

    let slow = t / 2;
    let fast = t * 2;
    let rounds = 50000;
    let iters = 50;

    println!("flag_bearer[threads = {t}, permits = {slow}]:");
    let semaphore = flag_bearer_tests::Semaphore::new(slow);
    async_bench(t, rounds, iters, semaphore, async |s| {
        black_box(s.try_acquire());
    });

    println!("flag_bearer[threads = {t}, permits = {fast}]:");
    let semaphore = flag_bearer_tests::Semaphore::new(fast);
    async_bench(t, rounds, iters, semaphore, async |s| {
        black_box(s.try_acquire());
    });

    println!("flag_bearer[threads = 2, permits = 64]:");
    let semaphore = flag_bearer_tests::Semaphore::new(64);
    async_bench(2, rounds, iters, semaphore, async |s| {
        black_box(s.try_acquire());
    });

    println!("tokio[threads = {t}, permits = {slow}]:");
    let semaphore = tokio::sync::Semaphore::new(slow);
    async_bench(t, rounds, iters, semaphore, async |s| {
        drop(black_box(s.try_acquire()));
    });

    println!("tokio[threads = {t}, permits = {fast}]:");
    let semaphore = tokio::sync::Semaphore::new(fast);
    async_bench(t, rounds, iters, semaphore, async |s| {
        drop(black_box(s.try_acquire()));
    });

    println!("tokio[threads = 2, permits = 64]:");
    let semaphore = tokio::sync::Semaphore::new(64);
    async_bench(2, rounds, iters, semaphore, async |s| {
        drop(black_box(s.try_acquire()));
    });
}
