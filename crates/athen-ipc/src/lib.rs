//! IPC transport layer for Athen multi-process architecture.
//!
//! Provides Unix socket (Linux/macOS) transports with length-prefixed
//! JSON codec for `IpcMessage` communication between processes.
//!
//! # Architecture
//!
//! - [`codec`] — Encode/decode `IpcMessage` with 4-byte length-prefixed JSON framing
//! - [`transport`] — `IpcTransport` trait and `UnixTransport` implementation
//! - [`server`] — `IpcServer` (coordinator) and `IpcClient` (monitors/agents/UI)
//!
//! # Platform support
//!
//! Unix-only today — the transport layer is built on `tokio::net::UnixStream`.
//! Windows support requires a Named Pipe implementation; until that lands,
//! the entire crate is gated on `cfg(unix)` so Windows workspace builds
//! still succeed. Nothing in the workspace consumes `athen-ipc` yet — the
//! multi-process architecture is planned but not wired — so this gate has
//! no runtime impact.

#![cfg(unix)]

pub mod codec;
pub mod server;
pub mod transport;

// Re-export main types for convenience.
pub use server::{IpcClient, IpcServer};
pub use transport::{IpcTransport, UnixTransport};
