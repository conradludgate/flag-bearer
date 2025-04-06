//! A crate for generic semaphore performing asynchronous permit acquisition.
//!
//! A semaphore maintains a set of permits. Permits are used to synchronize
//! access to a shared resource. A semaphore differs from a mutex in that it
//! can allow more than one concurrent caller to access the shared resource at a
//! time.
//!
//! When `acquire` is called and the semaphore has remaining permits, the
//! function immediately returns a permit. However, if no remaining permits are
//! available, `acquire` (asynchronously) waits until an outstanding permit is
//! dropped. At this point, the freed permit is assigned to the caller.
//!
//! This `Semaphore` is fair, and supports both FIFO and LIFO modes.
//! * In FIFO mode, this fairness means that permits are given out in the order
//!   they were requested.
//! * In LIFO mode, this fairness means that permits are given out in the reverse
//!   order they were requested.
//!
//! This fairness is also applied when `acquire` with high 'parameters' gets
//! involved, so if a call to `acquire` at the end of the queue requests
//! more permits than currently available, this can prevent another call to `acquire`
//! from completing, even if the semaphore has enough permits to complete it.
//!
//! This semaphore is generic, which means you can customise the state.
//! Examples:
//! * Using two counters, you can immediately remove permits,
//!   while there are some still in flight. This might be useful
//!   if you want to remove concurrency if failures are detected.
//! * There might be multiple quantities you want to limit over.
//!   Stacking multiple semaphores can be awkward and risk deadlocks.
//!   Instead, making the state contain all those quantities combined
//!   can simplify the queueing.
//!
//! ## Performance
//!
//! My performance testing has shown that `flag_bearer`'s [`Semaphore`] is competitive
//! with [`tokio::sync::Semaphore`](https://docs.rs/tokio/latest/tokio/sync/struct.Semaphore.html).
//!
//! The only exception is in the case when `try_acquire` is called with no permits available,
//! `tokio` only needs a single atomic read for this case, whereas `flag_bearer` still needs to acquire a lock.
//! This is measurable, but mostly in the contended usecase.
//!
//! ## Examples
//!
//! ### A Semaphore like `tokio::sync::Semaphore`
//!
//! ```
//! #[derive(Debug)]
//! struct SemaphoreCounter(usize);
//!
//! impl flag_bearer::SemaphoreState for SemaphoreCounter {
//!     /// Number of permits to acquire
//!     type Params = usize;
//!
//!     /// Number of permits that have been acquired
//!     type Permit = usize;
//!
//!     fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params> {
//!         if let Some(available) = self.0.checked_sub(params) {
//!             self.0 = available;
//!             Ok(params)
//!         } else {
//!             Err(params)
//!         }
//!     }
//!
//!     fn release(&mut self, permit: Self::Permit) {
//!         self.0 = self.0.checked_add(permit).unwrap()
//!     }
//! }
//!
//! # pollster::block_on(async {
//! // create a new FIFO semaphore with 20 permits
//! let semaphore = flag_bearer::SemaphoreBuilder::fifo().closeable().with_state(SemaphoreCounter(20));
//!
//! // acquire a token
//! let _permit = semaphore.acquire(1).await.expect("semaphore shouldn't be closed");
//!
//! // add 20 more permits
//! semaphore.with_state(|s| s.0 += 20);
//!
//! // release a token
//! drop(_permit);
//!
//! // close a semaphore
//! semaphore.close();
//! # })
//! ```
//!
//! ### A more complex usecase with the ability to update limits on demand
//!
//! `tokio`'s Semaphore allows adding permits on demand, but it doesn't support removing permits.
//! You have to `forget` the permits that are already acquired, and if you don't have those on hand,
//! you will have to spawn a task to acquire them.
//!
//! With the following construction, we can define a state that allows removing permits at will.
//!
//! ```
//! #[derive(Debug)]
//! struct Utilisation {
//!     taken: usize,
//!     limit: usize
//! }
//!
//! impl flag_bearer::SemaphoreState for Utilisation {
//!     type Params = ();
//!     type Permit = ();
//!
//!     fn acquire(&mut self, p: Self::Params) -> Result<Self::Permit, Self::Params> {
//!         if self.taken < self.limit {
//!             self.taken += 1;
//!             Ok(p)
//!         } else {
//!             Err(p)
//!         }
//!     }
//!
//!     fn release(&mut self, _: Self::Permit) {
//!         self.taken -= 1;
//!     }
//! }
//!
//! impl Utilisation {
//!     pub fn new(tokens: usize) -> Self {
//!         Self { limit: tokens, taken: 0 }
//!     }
//!     pub fn add_tokens(&mut self, x: usize) {
//!         self.limit += x;
//!     }
//!     pub fn remove_tokens(&mut self, x: usize) {
//!         self.limit -= x;
//!     }
//! }
//!
//! # pollster::block_on(async {
//! // create a new FIFO semaphore with 20 tokens
//! let semaphore = flag_bearer::SemaphoreBuilder::fifo().with_state(Utilisation::new(20));
//!
//! // acquire a permit
//! let _permit = semaphore.must_acquire(()).await;
//!
//! // remove 10 tokens
//! semaphore.with_state(|s| s.remove_tokens(10));
//!
//! // release a permit
//! drop(_permit);
//! # })
//! ```
//!
//! ### A LIFO semaphore which tracks multiple values at once
//!
//! Last-in, first-out is an unfair strategy of queueing, where the latest queued task
//! will be the first one to get the permit. This might seem like a bad strategy, but
//! if high latency is considered a failure in your applications, this can reduce
//! the failure rate. In a FIFO setting, you might have a P50 = 50ms, P99 = 100ms.
//! The same in a LIFO setting could be P50 = 10ms, P99 = 500ms. If 50ms half the time
//! is too slow for your application, then switching to LIFO could help.
//! <https://encore.dev/blog/queueing#lifo>
//!
//! Additionally to LIFO, Our [`SemaphoreState`] allows us to track multiple fields at once.
//! Let's imagine we want to limit in-flight requests, as well as in-flight request body allocations.
//! We can put the two counters in a single state object.
//!
//! ```
//! #[derive(Debug)]
//! struct Utilisation {
//!     requests: usize,
//!     bytes: u64,
//! }
//!
//! struct Request {
//!     bytes: u64,
//! }
//!
//! impl flag_bearer::SemaphoreState for Utilisation {
//!     type Params = Request;
//!     type Permit = Request;
//!
//!     fn acquire(&mut self, p: Self::Params) -> Result<Self::Permit, Self::Params> {
//!         if self.requests >= 1 && self.bytes >= p.bytes {
//!             self.requests -= 1;
//!             self.bytes -= p.bytes;
//!             Ok(p)
//!         } else {
//!             Err(p)
//!         }
//!     }
//!
//!     fn release(&mut self, p: Self::Permit) {
//!         self.requests += 1;
//!         self.bytes += p.bytes;
//!     }
//! }
//!
//! # pollster::block_on(async {
//! // create a new LIFO semaphore with support for 1 MB and 20 requests
//! let semaphore = flag_bearer::SemaphoreBuilder::lifo().with_state(Utilisation {
//!     requests: 20,
//!     bytes: 1024 * 1024,
//! });
//!
//! // acquire a permit for a request with 64KB
//! let _permit = semaphore.must_acquire(Request { bytes: 64 * 1024 }).await;
//! # })
//! ```
//!
//! ### A connection pool
//!
//! A connection pool can work quite like a semaphore sometimes. There's a limited number of connections
//! and you don't want too many at one time. Our [`SemaphoreState::Permit`]s don't need to be Plain-Old-Data,
//! so we can use them to hold connection objects too.
//!
//! This example still allows creating new connections on demand, if they are needed in high load cases, as well as re-creating
//! connections if they fail.
//!
//! ```
//! #[derive(Debug)]
//! struct Connection {
//!     // ...
//! }
//!
//! impl Connection {
//!     /// creates a new conn
//!     pub fn new() -> Self { Self {} }
//!     /// checks the connection liveness
//!     pub async fn check(&mut self) -> bool { true }
//!     /// do something
//!     pub async fn query(&mut self) {}
//! }
//!
//! #[derive(Debug, Default)]
//! struct Pool {
//!     conns: Vec<Connection>,
//! }
//!
//! impl flag_bearer::SemaphoreState for Pool {
//!     type Params = ();
//!     type Permit = Connection;
//!
//!     fn acquire(&mut self, p: Self::Params) -> Result<Self::Permit, Self::Params> {
//!         self.conns.pop().ok_or(p)
//!     }
//!
//!     fn release(&mut self, p: Self::Permit) {
//!         self.conns.push(p);
//!     }
//! }
//!
//! # async fn timeout<F: std::future::Future>(_f: F, _duration: std::time::Duration) -> Result<F::Output, ()> { Err(()) }
//! async fn acquire_conn(s: &flag_bearer::Semaphore<Pool>) -> flag_bearer::Permit<'_, Pool> {
//!     let d = std::time::Duration::from_millis(200);
//!     if let Ok(mut permit) = timeout(s.must_acquire(()), d).await {
//!         if permit.check().await {
//!             // We acquired a permit, and the liveness check succeeded.
//!             // Return the permit.
//!             return permit;
//!         }
//!
//!         // do not return this connection to the semaphore, as it is broken.
//!         flag_bearer::Permit::take(permit);
//!     }
//!
//!     // There was a timeout, or the connection liveness check failed.
//!     // Create a new connection and permit.
//!     let c = Connection::new();
//!     flag_bearer::Permit::out_of_thin_air(&s, c)
//! }
//!
//! # pollster::block_on(async {
//! // Create a new LIFO connection pool.
//! // We choose LIFO here because we create new connections on timeout, and LIFO is
//! // more likely to have timeouts but on fewer tasks, which ends up improving our
//! // performance.
//! let semaphore = flag_bearer::SemaphoreBuilder::lifo().with_state(Pool::default());
//!
//! let mut conn = acquire_conn(&semaphore).await;
//!
//! // access the inner conn
//! conn.query().await;
//! # })
//! ```

#![no_std]
#![warn(
    unsafe_op_in_unsafe_fn,
    clippy::missing_safety_doc,
    clippy::multiple_unsafe_ops_per_block,
    clippy::undocumented_unsafe_blocks
)]

use core::marker::PhantomData;

#[cfg(test)]
extern crate std;

use mutex::Mutex;
use queue::QueueState;

mod closeable;
mod drop_wrapper;
mod mutex;
mod permit;
mod queue;

pub use closeable::{Closeable, IsCloseable, Uncloseable};
pub use permit::Permit;
pub use queue::acquire::{AcquireError, TryAcquireError};

/// A Builder for [`Semaphore`]s.
pub struct SemaphoreBuilder<C: IsCloseable = Uncloseable> {
    order: FairOrder,
    closeable: PhantomData<C>,
}

impl SemaphoreBuilder {
    /// Create a new first-in-first-out semaphore builder
    pub fn fifo() -> Self {
        Self {
            order: FairOrder::Fifo,
            closeable: PhantomData,
        }
    }

    /// Create a new last-in-first-out semaphore builder
    pub fn lifo() -> Self {
        Self {
            order: FairOrder::Lifo,
            closeable: PhantomData,
        }
    }

    /// The semaphore this builder constructs will be closeable.
    pub fn closeable(self) -> SemaphoreBuilder<Closeable> {
        SemaphoreBuilder {
            order: self.order,
            closeable: PhantomData,
        }
    }
}

impl<C: IsCloseable> SemaphoreBuilder<C> {
    /// Build the [`Semaphore`] with the provided initial state.
    pub fn with_state<S: SemaphoreState>(self, state: S) -> Semaphore<S, C> {
        Semaphore::new_inner(state, self.order)
    }
}

/// The trait defining how [`Semaphore`]s behave.
///
/// The usage of this state is as follows:
/// ```
/// # use flag_bearer::SemaphoreState;
/// # use std::sync::Mutex;
/// fn get_permit<S: SemaphoreState>(s: &Mutex<S>, mut params: S::Params) -> S::Permit {
///     loop {
///         let mut s = s.lock().unwrap();
///         match s.acquire(params) {
///             Ok(permit) => break permit,
///             Err(p) => params = p,
///         }
///
///         // sleep/spin/yield until time to try again
///     }
/// }
///
/// fn return_permit<S: SemaphoreState>(s: &Mutex<S>, permit: S::Permit) {
///     s.lock().unwrap().release(permit);
/// }
/// ```
///
/// This is + async queueing is implemented for you by [`Semaphore`], with RAII returning via [`Permit`].
pub trait SemaphoreState {
    /// What type is used to request permits.
    ///
    /// An example of this could be `usize` for a counting semaphore,
    /// if you want to support `acquire_many` type requests.
    type Params;

    /// The type representing the current permit allocation.
    ///
    /// If you have a counting semaphore, this could be the number
    /// of permits acquired. If this is more like a connection pool,
    /// this could be a specific object allocation.
    type Permit;

    /// Acquire a permit given the params.
    ///
    /// If a permit could not be acquired with the params, return an error with the
    /// original params back.
    fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params>;

    /// Return the permit back to the semaphore.
    ///
    /// Note: This is not guaranteed to be called for every acquire call.
    /// Permits can be modified or forgotten.
    fn release(&mut self, permit: Self::Permit);
}

/// Generic semaphore performing asynchronous permit acquisition.
///
/// A semaphore maintains a set of permits. Permits are used to synchronize
/// access to a shared resource. A semaphore differs from a mutex in that it
/// can allow more than one concurrent caller to access the shared resource at a
/// time.
///
/// When `acquire` is called and the semaphore has remaining permits, the
/// function immediately returns a permit. However, if no remaining permits are
/// available, `acquire` (asynchronously) waits until an outstanding permit is
/// dropped. At this point, the freed permit is assigned to the caller.
///
/// This `Semaphore` is fair, and supports both FIFO and LIFO modes.
/// * In FIFO mode, this fairness means that permits are given out in the order
///   they were requested.
/// * In LIFO mode, this fairness means that permits are given out in the reverse
///   order they were requested.
///
/// This fairness is also applied when `acquire` with high 'parameters' gets
/// involved, so if a call to `acquire` at the end of the queue requests
/// more permits than currently available, this can prevent another call to `acquire`
/// from completing, even if the semaphore has enough permits to complete it.
///
/// This semaphore is generic, which means you can customise the state.
/// Examples:
/// * Using two counters, you can immediately remove permits,
///   while there are some still in flight. This might be useful
///   if you want to remove concurrency if failures are detected.
/// * There might be multiple quantities you want to limit over.
///   Stacking multiple semaphores can be awkward and risk deadlocks.
///   Instead, making the state contain all those quantities combined
///   can simplify the queueing.
pub struct Semaphore<S: SemaphoreState + ?Sized, C: IsCloseable = Uncloseable> {
    order: FairOrder,
    state: Mutex<QueueState<S, C>>,
}

impl<S: SemaphoreState, C: IsCloseable> Semaphore<S, C> {
    fn new_inner(state: S, order: FairOrder) -> Self {
        let state = QueueState::new(state);
        Self {
            state: Mutex::new(state),
            order,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum FairOrder {
    /// Last in, first out.
    /// Increases tail latencies, but can have better average performance.
    Lifo,
    /// First in, first out.
    /// Fairer option, but can have cascading failures if queue processing is slow.
    Fifo,
}

impl FairOrder {
    fn is_lifo(&self) -> bool {
        matches!(self, FairOrder::Lifo)
    }
}

impl<S: SemaphoreState + core::fmt::Debug> core::fmt::Debug for Semaphore<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_struct("Semaphore");
        d.field("order", &self.order);
        match self.state.try_lock() {
            Some(guard) => {
                d.field("state", &guard.state);
            }
            None => {
                d.field("state", &format_args!("<locked>"));
            }
        }
        d.finish_non_exhaustive()
    }
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable> Semaphore<S, C> {
    /// Access the state with mutable access.
    ///
    /// This gives direct access to the state, be careful not to
    /// break any of your own state invariants. You can use this
    /// to peek at the current state, or to modify it, eg to add or
    /// remove permits from the semaphore.
    pub fn with_state<R>(&self, f: impl FnOnce(&mut S) -> R) -> R {
        let mut state = self.state.lock();
        let res = f(&mut state.state);
        state.check();
        res
    }

    /// Check if the semaphore is closed
    pub fn is_closed(&self) -> bool {
        C::CLOSE && self.state.lock().is_closed()
    }
}

impl<S: SemaphoreState + ?Sized> Semaphore<S, Closeable> {
    /// Close the semaphore.
    ///
    /// All tasks currently waiting to acquire a token will immediately stop.
    /// No new acquire attempts will succeed.
    pub fn close(&self) {
        self.state.lock().close();
    }
}

#[cfg(test)]
mod test {
    #[derive(Debug)]
    struct Dummy;

    impl crate::SemaphoreState for Dummy {
        type Params = ();
        type Permit = ();

        fn acquire(&mut self, _params: Self::Params) -> Result<Self::Permit, Self::Params> {
            Ok(())
        }

        fn release(&mut self, _permit: Self::Permit) {}
    }

    #[test]
    fn debug() {
        let s = crate::SemaphoreBuilder::fifo().with_state(Dummy);
        let s = std::format!("{s:?}");
        assert_eq!(s, "Semaphore { order: Fifo, state: Dummy, .. }");
    }

    #[test]
    fn is_closed() {
        let s = crate::SemaphoreBuilder::fifo().with_state(Dummy);
        assert!(!s.is_closed());

        let s = crate::SemaphoreBuilder::fifo()
            .closeable()
            .with_state(Dummy);
        assert!(!s.is_closed());
        s.close();
        assert!(s.is_closed());
    }

    #[derive(Debug)]
    struct NeverSucceeds;

    impl crate::SemaphoreState for NeverSucceeds {
        type Params = ();
        type Permit = ();

        fn acquire(&mut self, _params: Self::Params) -> Result<Self::Permit, Self::Params> {
            Err(())
        }

        fn release(&mut self, _permit: Self::Permit) {}
    }

    #[cfg(loom)]
    #[test]
    fn concurrent_closed() {
        loom::model(|| {
            use std::sync::Arc;
            let s = Arc::new(
                crate::SemaphoreBuilder::fifo()
                    .closeable()
                    .with_state(NeverSucceeds),
            );

            let s2 = s.clone();
            let handle = loom::thread::spawn(move || {
                loom::future::block_on(async move { s2.acquire(()).await.unwrap_err() })
            });

            s.close();

            handle.join().unwrap();
        });
    }
}
