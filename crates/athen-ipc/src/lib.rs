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

pub mod codec;
pub mod server;
pub mod transport;

// Re-export main types for convenience.
pub use server::{IpcClient, IpcServer};
pub use transport::{IpcTransport, UnixTransport};
