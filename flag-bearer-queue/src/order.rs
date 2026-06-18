//! Queue ordering policies — see [`Order`].

/// Decides which end of the wait queue a newly-arriving task joins.
///
/// Permits are always handed out from the front, so an order that enqueues at the
/// front behaves LIFO and one that enqueues at the back behaves FIFO. Implement
/// this trait to provide a custom policy (e.g. a randomized one owning its own RNG).
pub trait Order {
    /// Should a newly-arriving waiter join at the front (LIFO end) rather than the
    /// back (FIFO end), given how many tasks are already waiting?
    ///
    /// Takes `&mut self` so stateful policies can advance their state per decision.
    fn enqueue_front(&mut self, queued: usize) -> bool;
}

/// The default ordering: strict first-in-first-out or last-in-first-out.
#[derive(Debug, Clone, Copy)]
pub enum FairOrder {
    /// Last in, first out. Increases tail latencies, but can degrade better under
    /// contention.
    Lifo,
    /// First in, first out. Fairer and predictable, but can cause cascading
    /// failures if queue processing is slow.
    Fifo,
}

impl Order for FairOrder {
    #[inline]
    fn enqueue_front(&mut self, _queued: usize) -> bool {
        matches!(self, FairOrder::Lifo)
    }
}
