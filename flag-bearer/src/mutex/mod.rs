cfg_if::cfg_if! {
    if #[cfg(all(test, loom))] {
        #[path = "loom.rs"]
        mod shim;
    } else if #[cfg(target_os = "linux")] {
        // linux seems to have better perf with just futex than with parking_lot.
        #[path = "futex.rs"]
        mod shim;
    } else {
        #[path = "parking_lot.rs"]
        mod shim;
    }
}

pub use shim::Mutex;
