//! A connection pool built on a semaphore: the permits *are* the connections.
//!
//! `SemaphoreState::Permit` doesn't have to be a count — it can hold real data.
//! So the pool hands back an actual connection and reclaims it when the permit
//! drops. LIFO keeps recently-used (warm) connections hot. Connections can be
//! minted on demand with `Permit::out_of_thin_air`, and broken ones dropped
//! from the pool with `Permit::take`.
//!
//! Run with: `cargo run --example connection_pool`

use std::sync::atomic::{AtomicU32, Ordering};

use flag_bearer::{Builder, Permit, Semaphore, SemaphoreState};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

#[derive(Debug)]
struct Connection {
    id: u32,
}

impl Connection {
    fn open() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        println!("  (opened connection #{id})");
        Self { id }
    }
}

#[derive(Default)]
struct Pool {
    idle: Vec<Connection>,
}

impl SemaphoreState for Pool {
    type Params = ();
    type Permit = Connection;

    fn acquire(&mut self, (): ()) -> Result<Connection, ()> {
        self.idle.pop().ok_or(())
    }

    fn release(&mut self, conn: Connection) {
        self.idle.push(conn);
    }
}

/// Check out a connection, minting a fresh one if the pool is empty.
fn checkout(pool: &Semaphore<Pool>) -> Permit<'_, Pool> {
    match pool.try_acquire(()) {
        Ok(conn) => conn,
        Err(_) => Permit::out_of_thin_air(pool, Connection::open()),
    }
}

fn main() {
    // LIFO so the most-recently-returned (warm) connection is reused first.
    let pool = Builder::lifo().with_state(Pool::default());

    // Seed the pool with one connection.
    pool.with_state(|p| p.idle.push(Connection::open()));

    // Two checkouts: one reuses the seeded connection, the other is minted.
    let a = checkout(&pool);
    let b = checkout(&pool);
    println!("checked out #{} and #{}", a.id, b.id);

    // Return both to the pool.
    drop(a);
    drop(b);

    // LIFO hands back the most-recently-returned connection (#b) first.
    let c = checkout(&pool);
    println!("reused #{} (most recently returned)", c.id);

    // This one turned out to be broken — discard it instead of returning it.
    println!("discarding broken #{}", c.id);
    Permit::take(c);

    println!(
        "idle connections left in pool: {}",
        pool.with_state(|p| p.idle.len())
    );
}
