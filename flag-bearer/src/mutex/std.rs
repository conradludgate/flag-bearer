pub struct Mutex<T: ?Sized>(std::sync::Mutex<T>);

impl<T> Mutex<T> {
    pub fn new(t: T) -> Self {
        Self(std::sync::Mutex::new(t))
    }
}

impl<T: ?Sized> Mutex<T> {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, T> {
        // ignoring poison support for now.
        self.0.lock().unwrap_or_else(|g| g.into_inner())
    }

    pub fn try_lock(&self) -> Option<std::sync::MutexGuard<'_, T>> {
        self.0.try_lock().ok()
    }
}
