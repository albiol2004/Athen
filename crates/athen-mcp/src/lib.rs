//! MCP runtime for Athen.
//!
//! Two pieces:
//! - [`catalog`] — the curated, branded list of MCPs the user can enable
//!   (e.g. "Files"). Hardcoded for now; later entries may be downloadable.
//! - [`registry`] — runtime state: which catalog entries are enabled, the
//!   per-entry config, and the lazy-spawned child processes that back them.

pub mod catalog;
pub mod registry;

pub use catalog::{builtin_catalog, lookup};
pub use registry::{EnabledEntry, McpRegistry};
