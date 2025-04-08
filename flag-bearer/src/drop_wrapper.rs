//! Jank proof-of-concept for a type that can take owned values during drop.
#![allow(unsafe_code)]

use core::mem::ManuallyDrop;

pub trait Drop2 {
    type T;
    fn drop(&mut self, t: Self::T);
}

pub struct DropWrapper<S: Drop2> {
    pub t: ManuallyDrop<S::T>,
    pub s: S,
}

impl<S: Drop2> Drop for DropWrapper<S> {
    fn drop(&mut self) {
        // Safety: We will not touch `t` after this call.
        self.s.drop(unsafe { ManuallyDrop::take(&mut self.t) });
    }
}

impl<S: Drop2> DropWrapper<S> {
    pub fn new(s: S, t: S::T) -> Self {
        Self {
            t: ManuallyDrop::new(t),
            s,
        }
    }

    pub fn take(self) -> S::T {
        let mut this = ManuallyDrop::new(self);
        let Self { t, s } = &mut *this;

        // Safety: we do not touch `s` again, and it is valid for drop
        unsafe { core::ptr::drop_in_place(s) };

        // Safety: we do not touch `t` again
        unsafe { ManuallyDrop::take(t) }
    }
}
