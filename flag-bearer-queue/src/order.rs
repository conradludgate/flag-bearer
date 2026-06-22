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

/// A size-gated blend of FIFO and LIFO: behaves as FIFO while at most `lifo_above`
/// tasks are waiting, and switches to LIFO once the queue grows beyond that.
///
/// This keeps FIFO's predictability under light load while degrading like LIFO
/// under contention. Note that under sustained overload the FIFO backlog can be
/// starved — this gives up FIFO's no-starvation guarantee.
#[derive(Debug, Clone, Copy)]
pub struct Hybrid {
    /// The queue length above which new waiters are enqueued LIFO.
    pub lifo_above: usize,
}

impl Order for Hybrid {
    #[inline]
    fn enqueue_front(&mut self, queued: usize) -> bool {
        queued > self.lifo_above
    }
}
