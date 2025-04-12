use std::{
    ops::{Deref, DerefMut},
    pin::pin,
    sync::Arc,
    task::Poll,
    time::Duration,
};

use flag_bearer::{OwnedPermit, Semaphore, SemaphoreState, Uncloseable};
use flag_bearer_mutex::{RawMutex, lock_api::Mutex};
use flag_bearer_queue::{SemaphoreQueue, acquire::FairOrder};
use tokio_postgres::{Client, Config, Error, Socket, tls::MakeTlsConnect};

#[derive(Default)]
struct PoolState {
    max: usize,
    taken: usize,
    conns: Vec<Client>,
}

impl SemaphoreState for PoolState {
    type Params = ();
    type Permit = Client;

    fn acquire(&mut self, (): Self::Params) -> Result<Self::Permit, Self::Params> {
        if self.taken >= self.max {
            return Err(());
        }

        while let Some(conn) = self.conns.pop() {
            if !conn.is_closed() {
                self.taken += 1;
                return Ok(conn);
            }
        }

        Err(())
    }

    fn release(&mut self, conn: Self::Permit) {
        self.taken -= 1;
        if self.conns.len() < self.max && !conn.is_closed() {
            self.conns.push(conn);
        }
    }
}

impl PoolState {
    fn len(&self) -> usize {
        self.conns.len() + self.taken
    }

    fn empty(&self) -> bool {
        self.len() == 0
    }

    fn set_capacity(&mut self, capacity: usize) {
        self.max = capacity;
        while self.conns.len() > capacity {
            self.conns.pop();
        }
    }
}

pub struct Pool<T> {
    config: Config,
    queue: Arc<Mutex<RawMutex, SemaphoreQueue<PoolState, Uncloseable>>>,
    debounce: Duration,
    tls: T,
}

impl<T> Pool<T> {
    pub fn new(config: Config, tls: T) -> Self {
        Self {
            queue: Arc::new(Mutex::new(SemaphoreQueue::new(PoolState {
                max: 10,
                taken: 0,
                conns: vec![],
            }))),
            debounce: config
                .get_connect_timeout()
                .copied()
                .unwrap_or(Duration::from_millis(50)),
            config,
            tls,
        }
    }

    pub fn debounce(&mut self, timeout: Duration) -> &mut Self {
        self.debounce = timeout;
        self
    }

    pub fn max_conns(&mut self, conns: usize) -> &mut Self {
        self.queue.lock().with_state(|s| s.set_capacity(conns));
        self
    }
}

impl<T> Pool<T>
where
    T: MakeTlsConnect<Socket> + Clone,
    T::Stream: Send + 'static,
{
    pub async fn acquire(&self) -> Result<Connection, Error> {
        let mut sleep = pin!(tokio::time::sleep(self.debounce));
        let res =
            SemaphoreQueue::acquire_while(&self.queue, (), FairOrder::Fifo, move |s, (), cx| {
                if s.len() < s.max && (s.empty() || sleep.as_mut().poll(cx).is_ready()) {
                    // we can open a new conn now
                    s.taken += 1;
                    return Poll::Ready(());
                }

                // wait until a connection is returned
                Poll::Pending
            })
            .await;

        if let Ok(permit) = res {
            // todo: use check_connection from https://github.com/sfackler/rust-postgres/pull/1229/files
            if permit.batch_execute("select 1").await.is_ok() {
                // We acquired a permit, and the liveness check succeeded.
                // Return the permit.
                return Ok(Connection {
                    permit: permit.into_owned_permit(self.store.clone()),
                });
            }

            // do not return this connection to the semaphore, as it is broken.
            flag_bearer::Permit::take(permit);
        }

        // There was a timeout, or the connection liveness check failed.
        // Create a new connection and permit.
        let (client, conn) = self.config.connect(self.tls.clone()).await?;
        tokio::spawn(conn);

        Ok(Connection {
            permit: OwnedPermit::out_of_thin_air(self.store.clone(), client),
        })
    }
}

pub struct Connection {
    permit: OwnedPermit<PoolState>,
}

impl Deref for Connection {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.permit
    }
}

impl DerefMut for Connection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.permit
    }
}

#[cfg(test)]
mod tests {
    use std::{str::FromStr, time::Duration};

    use tokio::time::timeout;
    use tokio_postgres::{Client, Config, NoTls};

    use crate::Pool;

    async fn get_pid(c: &Client) -> i32 {
        c.query_one("select pg_backend_pid()", &[])
            .await
            .unwrap()
            .get(0)
    }

    async fn fail_timeout(d: Duration, f: impl Future) {
        assert!(timeout(d, f).await.is_err());
    }

    #[tokio::test]
    async fn check() {
        let mut pool = Pool::new(
            Config::from_str("host=localhost port=5432 user=test password=test").unwrap(),
            NoTls,
        );
        pool.max_conns(2);
        pool.debounce(Duration::from_secs(1));

        let conn1 = pool.acquire().await.unwrap();

        // should debounce
        fail_timeout(Duration::from_millis(500), pool.acquire()).await;

        // but should succeed if given time.
        let conn2 = pool.acquire().await.unwrap();
        let conn2_pid = get_pid(&conn2).await;

        let _ = fail_timeout(Duration::from_secs(2), pool.acquire()).await;

        drop(conn2);
        conn1
            .execute("select pg_terminate_backend($1)", &[&conn2_pid])
            .await
            .unwrap();

        let conn3 = pool.acquire().await.unwrap();
        let conn3_pid = get_pid(&conn3).await;
        assert_ne!(conn2_pid, conn3_pid);
    }
}
