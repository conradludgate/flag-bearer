use flag_bearer::SemaphoreState;

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
    let pool = flag_bearer::new_lifo().with_state(Pool::default());

    pool.with_state(|s| {
        s.objects.push(Conn(0));
        s.objects.push(Conn(1));
    });

    let mut conn1 = pool.acquire(()).await.unwrap_or_else(|x| x.never());
    assert_eq!(conn1.0, 1);
    conn1.0 = 11;

    let mut conn0 = pool.acquire(()).await.unwrap_or_else(|x| x.never());
    assert_eq!(conn0.0, 0);
    conn0.0 = 10;

    drop(conn1);
    drop(conn0);

    let conn0 = pool.acquire(()).await.unwrap_or_else(|x| x.never());
    assert_eq!(conn0.0, 10);
}
