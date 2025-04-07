use crate::AcquireError;

pub(crate) mod private {
    pub trait Sealed {
        const CLOSE: bool;
        type Closed<P>;
    }
}

/// Whether the semaphore is closeable
pub trait IsCloseable: private::Sealed {
    /// The error returned during [`Semaphore::acquire`](crate::Semaphore::acquire).
    /// It will be a variant of [`AcquireError`].
    ///
    /// * [`Closeable`] semaphores return a [`AcquireError<P>`].
    /// * [`Uncloseable`] semaphores return [`AcquireError<Uncloseable>`].
    type AcquireError<P>;

    #[doc(hidden)]
    fn new_err<P>(params: P) -> Self::AcquireError<P>;
    #[doc(hidden)]
    fn map_err<P, R>(p: Self::Closed<P>, f: impl FnOnce(P) -> R) -> Self::AcquireError<R>;
}

/// Controls whether a [`Semaphore`](crate::Semaphore) is closeable.
pub enum Closeable {}

/// Controls whether a [`Semaphore`](crate::Semaphore) is not closeable.
pub enum Uncloseable {}

impl private::Sealed for Closeable {
    const CLOSE: bool = true;
    type Closed<P> = P;
}

impl IsCloseable for Closeable {
    type AcquireError<P> = AcquireError<P>;

    #[doc(hidden)]
    fn new_err<P>(params: P) -> Self::AcquireError<P> {
        AcquireError { params }
    }
    #[doc(hidden)]
    fn map_err<P, R>(p: Self::Closed<P>, f: impl FnOnce(P) -> R) -> Self::AcquireError<R> {
        AcquireError { params: f(p) }
    }
}

impl private::Sealed for Uncloseable {
    const CLOSE: bool = false;
    type Closed<P> = Self;
}

impl IsCloseable for Uncloseable {
    type AcquireError<P> = AcquireError<Uncloseable>;

    #[doc(hidden)]
    fn new_err<P>(_params: P) -> Self::AcquireError<P> {
        unreachable!()
    }
    #[doc(hidden)]
    fn map_err<P, R>(p: Self::Closed<P>, _f: impl FnOnce(P) -> R) -> Self::AcquireError<R> {
        match p {}
    }
}

impl core::fmt::Display for Uncloseable {
    fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {}
    }
}

impl core::fmt::Debug for Uncloseable {
    fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {}
    }
}

impl core::error::Error for Uncloseable {}
