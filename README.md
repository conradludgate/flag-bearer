# flag-bearer: a generic async semaphore

<!-- [![Tests](https://img.shields.io/github/actions/workflow/status/conradludgate/flag-bearer/test.yml?style=flat-square)](https://github.com/conradludgate/flag-bearer/actions/workflows/test.yml) -->
![License: Apache-2.0 OR MIT](https://img.shields.io/crates/l/flag-bearer?style=flat-square)
[![](https://img.shields.io/crates/v/flag-bearer?style=flat-square)](https://crates.io/crates/flag-bearer)
[![](https://img.shields.io/docsrs/flag-bearer/latest?style=flat-square)](https://docs.rs/flag-bearer/latest/flag-bearer/)

Semaphores are very useful primitives, but the default tokio Semaphore is limited in it's functionality.

This crate aims to fill a gap left by tokio, to have extra functionality to track to permits available.

## Examples

Run any of these with `cargo run -p flag-bearer-examples --example <name>`:

- [`dynamic_limit`](flag-bearer-examples/examples/dynamic_limit.rs) — a concurrency limiter whose limit can be raised *and lowered* at runtime, even below the number of permits in flight.
- [`multi_dimensional`](flag-bearer-examples/examples/multi_dimensional.rs) — capping concurrent requests *and* total in-flight bytes from a single semaphore (no deadlock-prone stacking).
- [`connection_pool`](flag-bearer-examples/examples/connection_pool.rs) — a LIFO pool whose permits *are* the connections, minting new ones on demand and discarding broken ones.
- [`aimd`](flag-bearer-examples/examples/aimd.rs) — an adaptive concurrency limiter (additive-increase / multiplicative-decrease) that tracks a downstream's overload point.
