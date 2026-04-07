//! LLM router and provider adapters for Athen.
//!
//! Plug-and-play LLM providers with profile-based routing,
//! failover chains, and budget management.

pub mod budget;
pub mod embeddings;
pub mod providers;
pub mod router;
