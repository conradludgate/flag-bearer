use std::sync::Arc;

use crate::{IsCloseable, Semaphore, SemaphoreState, Uncloseable, drop_wrapper::DropWrapper};

/// The drop-guard for semaphore permits.
/// Will ensure the permit is released when dropped.
#[must_use]
pub struct Permit<'a, S, C = Uncloseable>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
{
    inner: DropWrapper<SemWrapper<'a, S, C>>,
}

impl<S, C> core::fmt::Debug for Permit<'_, S, C>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
    S::Permit: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Permit")
            .field("permit", &self.inner.t)
            .finish_non_exhaustive()
    }
}

impl<S, C> core::ops::Deref for Permit<'_, S, C>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
{
    type Target = S::Permit;
    fn deref(&self) -> &Self::Target {
        &self.inner.t
    }
}

impl<S, C> core::ops::DerefMut for Permit<'_, S, C>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner.t
    }
}

impl<'a, S, C> Permit<'a, S, C>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
{
    /// Construct a new permit out of thin air, no waiting is required.
    ///
    /// This can violate the purpose of the semaphore, but is provided for convenience.
    /// It can be used to introduce new permits to the semaphore if desired, or it can be
    /// paired with [`Permit::take`] for niche use-cases where the semaphore lifetime
    /// gets in the way.
    pub fn out_of_thin_air(sem: &'a Semaphore<S, C>, permit: S::Permit) -> Self {
        Self {
            inner: DropWrapper::new(SemWrapper { sem }, permit),
        }
    }

    /// Get access to the associated semaphore
    pub fn semaphore(this: &Self) -> &'a Semaphore<S, C> {
        this.inner.s.sem
    }

    /// Do not release the permit to the semaphore.
    pub fn take(this: Self) -> S::Permit {
        this.inner.take()
    }

    /// Upgrade this permit into an [`OwnedPermit`].
    pub fn into_owned_permit(self, sem: Arc<Semaphore<S, C>>) -> OwnedPermit<S, C> {
        debug_assert_eq!(
            Arc::as_ptr(&sem),
            core::ptr::from_ref(Self::semaphore(&self)),
            "semaphore mismatch!"
        );
        OwnedPermit {
            inner: DropWrapper::new(SemWrapperOwned { sem }, Self::take(self)),
        }
    }
}

struct SemWrapper<'a, S: SemaphoreState + ?Sized, C: IsCloseable> {
    sem: &'a Semaphore<S, C>,
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable> crate::drop_wrapper::Drop2
    for SemWrapper<'_, S, C>
{
    type T = S::Permit;

    fn drop(&mut self, permit: Self::T) {
        self.sem.with_state(|s| s.release(permit));
    }
}

/// The drop-guard for owned semaphore permits.
/// Will ensure the permit is released when dropped.
#[must_use]
pub struct OwnedPermit<S, C = Uncloseable>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
{
    inner: DropWrapper<SemWrapperOwned<S, C>>,
}

impl<S, C> core::fmt::Debug for OwnedPermit<S, C>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
    S::Permit: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Permit")
            .field("permit", &self.inner.t)
            .finish_non_exhaustive()
    }
}

impl<S, C> core::ops::Deref for OwnedPermit<S, C>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
{
    type Target = S::Permit;
    fn deref(&self) -> &Self::Target {
        &self.inner.t
    }
}

impl<S, C> core::ops::DerefMut for OwnedPermit<S, C>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
{
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner.t
    }
}

impl<S, C> OwnedPermit<S, C>
where
    S: SemaphoreState + ?Sized,
    C: IsCloseable,
{
    /// Construct a new permit out of thin air, no waiting is required.
    ///
    /// This can violate the purpose of the semaphore, but is provided for convenience.
    /// It can be used to introduce new permits to the semaphore if desired, or it can be
    /// paired with [`Permit::take`] for niche use-cases where the semaphore lifetime
    /// gets in the way.
    pub fn out_of_thin_air(sem: Arc<Semaphore<S, C>>, permit: S::Permit) -> Self {
        Self {
            inner: DropWrapper::new(SemWrapperOwned { sem }, permit),
        }
    }

    /// Get access to the associated semaphore
    pub fn semaphore(this: &Self) -> &Arc<Semaphore<S, C>> {
        &this.inner.s.sem
    }

    /// Do not release the permit to the semaphore.
    pub fn take(this: Self) -> S::Permit {
        this.inner.take()
    }
}

struct SemWrapperOwned<S: SemaphoreState + ?Sized, C: IsCloseable> {
    sem: Arc<Semaphore<S, C>>,
}

impl<S: SemaphoreState + ?Sized, C: IsCloseable> crate::drop_wrapper::Drop2
    for SemWrapperOwned<S, C>
{
    type T = S::Permit;

    fn drop(&mut self, permit: Self::T) {
        self.sem.with_state(|s| s.release(permit));
    }
}
