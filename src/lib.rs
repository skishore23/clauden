//! clauden — multi-account Claude OAuth rotating proxy (library surface).
//!
//! The binary in `main.rs` is a thin CLI over these modules; integration tests
//! drive the proxy directly via [`server::router`] + [`server::make_state`].

pub mod config;
pub mod login;
pub mod oauth;
pub mod server;
pub mod ui;
