#[cfg(all(test, loom))]
#[path ="loom.rs"]
mod shim;

// linux seems to have better perf with std's futex than with parking_lot.
#[cfg(all(not(all(test, loom)), target_os="linux"))]
#[path ="std.rs"]
mod shim;

#[cfg(all(not(all(test, loom)), not(target_os="linux")))]
#[path ="parking_lot.rs"]
mod shim;

pub use shim::Mutex;
