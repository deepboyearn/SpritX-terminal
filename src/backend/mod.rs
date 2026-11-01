//! Non-UI backends for the GPUI app.
//!
//! Each module is deliberately GPUI-free and unit-testable: persistence,
//! terminal block parsing, and the remote wire protocol.

pub mod config;
pub mod files;
pub mod git;
pub mod history;
pub mod osc;
pub mod remote;
pub mod remote_client;
pub mod remote_server;
