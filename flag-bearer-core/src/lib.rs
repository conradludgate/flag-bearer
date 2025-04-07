#![no_std]
#![warn(
    unsafe_op_in_unsafe_fn,
    clippy::missing_safety_doc,
    clippy::multiple_unsafe_ops_per_block,
    clippy::undocumented_unsafe_blocks
)]

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
    /// Permits can be modified or forgotten, or created at will.
    fn release(&mut self, permit: Self::Permit);
}
