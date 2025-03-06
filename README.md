# flag-bearer: a generic async semaphore

Semaphores are very useful primitives, but the default tokio Semaphore is limited in it's functionality.

This crate aims to fill a gap left by tokio, to have extra functionality to track to permits available.

## Example usecase

You want to limit number of active HTTP requests, as well as total buffer allocations for the body.

You could define the semaphore state like so.

```
#[derive(Debug)]
pub struct SemaphoreCounter {
    bytes: u64,
    requests: usize,
};

pub struct Request {
    bytes: u64,
}

impl SemaphoreState for SemaphoreCounter {
    type Params = Request;
    type Permit = Request;

    fn permits_available(&self) -> bool {
        self.requests > 0 && self.bytes > 0
    }

    fn acquire(&mut self, params: Self::Params) -> Result<Self::Permit, Self::Params> {
        if self.bytes >= params.bytes && self.requests > 0 {
            self.bytes -= params.bytes;
            self.requests -= 1;

            Ok(params)
        } else {
            Err(params)
        }
    }

    fn release(&mut self, permit: Self::Permit) {
        self.bytes += permit.bytes;
        self.requests += 1;
    }
}
```
