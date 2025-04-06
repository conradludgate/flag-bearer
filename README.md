# flag-bearer: a generic async semaphore

<!-- [![Tests](https://img.shields.io/github/actions/workflow/status/conradludgate/flag-bearer/test.yml?style=flat-square)](https://github.com/conradludgate/flag-bearer/actions/workflows/test.yml) -->
![License: Apache-2.0 OR MIT](https://img.shields.io/crates/l/flag-bearer?style=flat-square)
[![](https://img.shields.io/crates/v/flag-bearer?style=flat-square)](https://crates.io/crates/flag-bearer)
[![](https://img.shields.io/docsrs/flag-bearer/latest?style=flat-square)](https://docs.rs/flag-bearer/latest/flag-bearer/)

Semaphores are very useful primitives, but the default tokio Semaphore is limited in it's functionality.

This crate aims to fill a gap left by tokio, to have extra functionality to track to permits available.
