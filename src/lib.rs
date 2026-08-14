//! Library half of `reel`, so the binary and the integration tests in `tests/` can
//! both drive the same code. `src/main.rs` is a thin shell over this: terminal setup
//! plus the event loop.

pub mod app;
pub mod cache;
pub mod cli;
pub mod config;
pub mod edit;
pub mod files;
pub mod input;
pub mod mount;
pub mod notification;
pub mod probe;
pub mod requirements;
pub mod staging;
pub mod subtitle;
pub mod ui;
