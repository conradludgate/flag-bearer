cfg_if::cfg_if! {
    if #[cfg(all(test, loom))] {
        use core::marker::PhantomData;

        pub struct Mutex<R, T: ?Sized>(PhantomData<R>, loom::sync::Mutex<T>);

        impl<R, T> Mutex<R, T> {
            pub fn new(t: T) -> Self {
                Self(PhantomData, loom::sync::Mutex::new(t))
            }
        }

        impl<R, T: ?Sized> Mutex<R, T> {
            pub fn lock(&self) -> loom::sync::MutexGuard<'_, T> {
                // ignoring poison support for now.
                self.1.lock().unwrap_or_else(|g| g.into_inner())
            }

            pub fn try_lock(&self) -> Option<loom::sync::MutexGuard<'_, T>> {
                self.1.try_lock().ok()
            }
        }
    } else {
        pub use lock_api::Mutex;
    }
}
