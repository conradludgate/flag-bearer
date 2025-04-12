//! Core traits for the `flag-bearer` family of crates.
//!
//! It's expected that you consume `flag-bearer` crate directly, but this is
//! exposed for flexibilty of queue implementations.

#![no_std]
#![deny(unsafe_code)]

/// The trait defining how semaphores behave.
///
/// The usage of this state is as follows:
/// ```
/// # use flag_bearer_core::SemaphoreState;
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
    /// # Errors
    ///
    /// If a permit could not be acquired with the given params,
    /// return an error and the original params back.
    fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params>;

    /// Return the permit back to the semaphore.
    ///
    /// Note: This is not guaranteed to be called for every acquire call.
    /// Permits can be modified or forgotten, or created at will.
    fn release(&mut self, permit: Self::Permit);
}
