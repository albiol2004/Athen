//! Persistence port for the registered HTTP endpoints store.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::Result;
use crate::http_endpoint::RegisteredEndpoint;

/// Storage for `RegisteredEndpoint` rows.
///
/// Implementations must keep `name` unique (case-insensitive). The `get_by_name`
/// helper exists because the agent looks endpoints up by name, not UUID — the
/// UUID is the primary key for rename safety.
///
/// All methods are infallible at the schema level: missing rows return `None`,
/// not an error. SQL/IO failures bubble up as `AthenError`.
#[async_trait]
pub trait HttpEndpointStore: Send + Sync {
    async fn list(&self) -> Result<Vec<RegisteredEndpoint>>;

    async fn get(&self, id: Uuid) -> Result<Option<RegisteredEndpoint>>;

    /// Case-insensitive lookup by display name. Used by the `http_request`
    /// tool dispatcher.
    async fn get_by_name(&self, name: &str) -> Result<Option<RegisteredEndpoint>>;

    /// Insert or replace. Implementations stamp `created_at` on first insert
    /// and preserve it on replace.
    async fn upsert(&self, endpoint: &RegisteredEndpoint) -> Result<()>;

    async fn delete(&self, id: Uuid) -> Result<()>;

    /// Bump the call counter and `last_used` after a successful call.
    /// Failures are non-fatal — a counter row that races during a crash
    /// is fine; the call already happened.
    async fn record_call(&self, id: Uuid) -> Result<()>;

    /// Toggle the enabled flag without round-tripping the whole row.
    async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<()>;
}
