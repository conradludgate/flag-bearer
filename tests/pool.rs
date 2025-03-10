use flag_bearer::{Semaphore, SemaphoreState};

struct Conn(usize);

#[derive(Default)]
struct Pool {
    objects: Vec<Conn>,
}

impl SemaphoreState for Pool {
    type Params = ();
    type Permit = Conn;

    fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params> {
        self.objects.pop().ok_or(params)
    }

    fn release(&mut self, permit: Self::Permit) {
        self.objects.push(permit);
    }
}

#[tokio::test]
async fn pool() {
    let pool = Semaphore::new_lifo(Pool::default());

    pool.with_state(|s| {
        s.objects.push(Conn(0));
        s.objects.push(Conn(1));
    });

    let mut conn1 = pool.acquire(()).await.unwrap();
    assert_eq!(conn1.permit().0, 1);
    conn1.permit_mut().0 = 11;

    let mut conn0 = pool.acquire(()).await.unwrap();
    assert_eq!(conn0.permit().0, 0);
    conn0.permit_mut().0 = 10;

    drop(conn1);
    drop(conn0);

    let conn0 = pool.acquire(()).await.unwrap();
    assert_eq!(conn0.permit().0, 10);
}
