use std::{
    pin::pin,
    sync::Arc,
    task::{Context, Poll},
};

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
    pub struct Permit<'a>(flag_bearer::Permit<'a, SemaphoreCounter>);

    #[derive(Debug)]
    pub enum TryAcquireError {
        NoPermits,
        Closed,
    }

    impl Semaphore {
        pub const MAX_PERMITS: usize = usize::MAX;

        pub fn new(count: usize) -> Self {
            Self(flag_bearer::Semaphore::new_fifo(SemaphoreCounter(count)))
        }

        pub async fn acquire(&self) -> Permit<'_> {
            Permit(self.0.acquire(1).await)
        }

        pub async fn acquire_many(&self, n: usize) -> Permit<'_> {
            Permit(self.0.acquire(n).await)
        }

        pub fn try_acquire(&self) -> Result<Permit<'_>, TryAcquireError> {
            self.try_acquire_many(1)
        }

        pub fn try_acquire_many(&self, n: usize) -> Result<Permit<'_>, TryAcquireError> {
            match self.0.try_acquire(n) {
                Ok(permit) => Ok(Permit(permit)),
                Err(flag_bearer::TryAcquireError::NoPermits(_)) => Err(TryAcquireError::NoPermits),
                Err(flag_bearer::TryAcquireError::Closed(_)) => Err(TryAcquireError::Closed),
            }
        }

        pub fn try_acquire_many_unfair(&self, n: usize) -> Result<Permit<'_>, TryAcquireError> {
            match self.0.try_acquire_unfair(n) {
                Ok(permit) => Ok(Permit(permit)),
                Err(flag_bearer::TryAcquireError::NoPermits(_)) => Err(TryAcquireError::NoPermits),
                Err(flag_bearer::TryAcquireError::Closed(_)) => Err(TryAcquireError::Closed),
            }
        }

        pub fn add_permits(&self, n: usize) {
            self.0.with_state(|s| s.0 = s.0.checked_add(n).unwrap());
        }

        pub fn available_permits(&self) -> usize {
            self.0.with_state(|s| s.0)
        }
    }

    impl Permit<'_> {
        pub fn num_permits(&self) -> usize {
            *self.0.permit()
        }

        pub fn split(&mut self, n: usize) -> Option<Self> {
            let this = self.0.permit().checked_sub(n)?;
            *self.0.permit_mut() = this;
            Some(Self(flag_bearer::Permit::out_of_thin_air(
                self.0.semaphore(),
                n,
            )))
        }

        pub fn merge(&mut self, other: Self) {
            let a = self.0.semaphore() as *const _;
            let b = other.0.semaphore() as *const _;

            assert_eq!(a, b, "semaphores of each permit should match");

            *self.0.permit_mut() += other.0.take();
        }

        pub fn forget(self) {
            self.0.take();
        }
    }
}

use futures_testing::{Driver, TestCase, drive_fn};
use futures_util::task::noop_waker_ref;
pub use semaphore::*;

#[test]
fn no_permits() {
    // this should not panic
    Semaphore::new(0);
}

#[test]
fn try_acquire() {
    let sem = Semaphore::new(1);
    {
        let p1 = sem.try_acquire();
        assert!(p1.is_ok());
        let p2 = sem.try_acquire();
        assert!(p2.is_err());
    }
    let p3 = sem.try_acquire();
    assert!(p3.is_ok());
}

#[tokio::test]
async fn acquire() {
    let sem = Arc::new(Semaphore::new(1));
    let p1 = sem.try_acquire().unwrap();
    let sem_clone = sem.clone();
    let j = tokio::spawn(async move {
        let _p2 = sem_clone.acquire().await;
    });
    drop(p1);
    j.await.unwrap();
}

#[tokio::test]
async fn add_permits() {
    let sem = Arc::new(Semaphore::new(0));
    let sem_clone = sem.clone();
    let j = tokio::spawn(async move {
        let _p2 = sem_clone.acquire().await;
    });
    sem.add_permits(1);
    j.await.unwrap();
}

#[test]
fn forget() {
    let sem = Arc::new(Semaphore::new(1));
    {
        let p = sem.try_acquire().unwrap();
        assert_eq!(sem.available_permits(), 0);
        p.forget();
        assert_eq!(sem.available_permits(), 0);
    }
    assert_eq!(sem.available_permits(), 0);
    assert!(sem.try_acquire().is_err());
}

#[test]
fn merge() {
    let sem = Arc::new(Semaphore::new(3));
    {
        let mut p1 = sem.try_acquire().unwrap();
        assert_eq!(sem.available_permits(), 2);
        let p2 = sem.try_acquire_many(2).unwrap();
        assert_eq!(sem.available_permits(), 0);
        p1.merge(p2);
        assert_eq!(sem.available_permits(), 0);
    }
    assert_eq!(sem.available_permits(), 3);
}

#[test]
#[should_panic]
fn merge_unrelated_permits() {
    let sem1 = Arc::new(Semaphore::new(3));
    let sem2 = Arc::new(Semaphore::new(3));
    let mut p1 = sem1.try_acquire().unwrap();
    let p2 = sem2.try_acquire().unwrap();
    p1.merge(p2);
}

#[test]
fn split() {
    let sem = Semaphore::new(5);
    let mut p1 = sem.try_acquire_many(3).unwrap();
    assert_eq!(sem.available_permits(), 2);
    assert_eq!(p1.num_permits(), 3);
    let mut p2 = p1.split(1).unwrap();
    assert_eq!(sem.available_permits(), 2);
    assert_eq!(p1.num_permits(), 2);
    assert_eq!(p2.num_permits(), 1);
    let p3 = p1.split(0).unwrap();
    assert_eq!(p3.num_permits(), 0);
    drop(p1);
    assert_eq!(sem.available_permits(), 4);
    let p4 = p2.split(1).unwrap();
    assert_eq!(p2.num_permits(), 0);
    assert_eq!(p4.num_permits(), 1);
    assert!(p2.split(1).is_none());
    drop(p2);
    assert_eq!(sem.available_permits(), 4);
    drop(p3);
    assert_eq!(sem.available_permits(), 4);
    drop(p4);
    assert_eq!(sem.available_permits(), 5);
}

#[tokio::test]
async fn stress_test() {
    let sem = Arc::new(Semaphore::new(5));
    let mut join_handles = Vec::new();
    for _ in 0..1000 {
        let sem_clone = sem.clone();
        join_handles.push(tokio::spawn(async move {
            let _p = sem_clone.acquire().await;
        }));
    }
    for j in join_handles {
        j.await.unwrap();
    }
    // there should be exactly 5 semaphores available now
    let _p1 = sem.try_acquire().unwrap();
    let _p2 = sem.try_acquire().unwrap();
    let _p3 = sem.try_acquire().unwrap();
    let _p4 = sem.try_acquire().unwrap();
    let _p5 = sem.try_acquire().unwrap();
    assert!(sem.try_acquire().is_err());
}

#[test]
fn add_max_amount_permits() {
    let s = tokio::sync::Semaphore::new(0);
    s.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
    assert_eq!(s.available_permits(), tokio::sync::Semaphore::MAX_PERMITS);
}

#[test]
#[should_panic]
fn add_more_than_max_amount_permits1() {
    let s = tokio::sync::Semaphore::new(1);
    s.add_permits(tokio::sync::Semaphore::MAX_PERMITS);
}

#[test]
#[should_panic]
fn add_more_than_max_amount_permits2() {
    let s = Semaphore::new(Semaphore::MAX_PERMITS - 1);
    s.add_permits(1);
    s.add_permits(1);
}

// #[test]
// #[should_panic]
// fn panic_when_exceeds_maxpermits() {
//     let _ = Semaphore::new(Semaphore::MAX_PERMITS + 1);
// }

#[test]
fn no_panic_at_maxpermits() {
    let _ = Semaphore::new(Semaphore::MAX_PERMITS);
    let s = Semaphore::new(Semaphore::MAX_PERMITS - 1);
    s.add_permits(1);
}

#[test]
fn fifo_order() {
    let s = Semaphore::new(0);
    let mut a1 = pin!(s.acquire_many(3));
    let mut a2 = pin!(s.acquire_many(2));
    let mut a3 = pin!(s.acquire_many(1));

    let mut cx = Context::from_waker(noop_waker_ref());
    assert!(a1.as_mut().poll(&mut cx).is_pending());
    assert!(a2.as_mut().poll(&mut cx).is_pending());
    assert!(a3.as_mut().poll(&mut cx).is_pending());

    s.add_permits(1);
    assert!(a1.as_mut().poll(&mut cx).is_pending());
    assert!(a2.as_mut().poll(&mut cx).is_pending());
    assert!(a3.as_mut().poll(&mut cx).is_pending());

    s.add_permits(1);
    assert!(a1.as_mut().poll(&mut cx).is_pending());
    assert!(a2.as_mut().poll(&mut cx).is_pending());
    assert!(a3.as_mut().poll(&mut cx).is_pending());

    s.add_permits(1);
    assert!(a2.as_mut().poll(&mut cx).is_pending());
    assert!(a3.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(p1) = a1.poll(&mut cx) else {
        panic!()
    };

    assert!(a2.as_mut().poll(&mut cx).is_pending());
    assert!(a3.as_mut().poll(&mut cx).is_pending());

    drop(p1);
    let Poll::Ready(_p3) = a3.poll(&mut cx) else {
        panic!()
    };
    let Poll::Ready(_p2) = a2.poll(&mut cx) else {
        panic!()
    };
}

#[test]
fn fifo_unfair() {
    let s = Semaphore::new(1);
    let mut a1 = pin!(s.acquire_many(3));

    let mut cx = Context::from_waker(noop_waker_ref());
    assert!(a1.as_mut().poll(&mut cx).is_pending());

    assert!(s.try_acquire_many(1).is_err());
    assert!(s.try_acquire_many_unfair(1).is_ok());
}

struct SemaphoreTestCase;

impl<'b> TestCase<'b> for SemaphoreTestCase {
    type Args = usize;

    fn init<'a>(&self, args: &'a mut usize) -> (impl Driver<'b>, impl Future) {
        let semaphore = Arc::new(Semaphore::new(0));

        let semaphore2 = semaphore.clone();
        let mut total = 0usize;
        let driver = drive_fn(move |n| {
            if let Some(t) = total.checked_add(n) {
                total = t;
                semaphore2.add_permits(n);
            }
        });

        let n = *args;
        let future = async move {
            semaphore.acquire_many(n).await;
        };

        (driver, future)
    }
}

#[test]
fn arbtest() {
    futures_testing::tests(SemaphoreTestCase).run();
}
