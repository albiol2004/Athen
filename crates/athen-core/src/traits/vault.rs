//! Credential vault port. Implementations live in `athen-vault`.

use async_trait::async_trait;

use crate::error::Result;

/// Encrypted at-rest storage for secrets (API keys, passwords, OAuth tokens).
///
/// Secrets are addressed by `(scope, key)`:
/// - `scope` is a logical namespace such as `endpoint:jina`, `imap:gmail`, or
///   `oauth:google`. Free-form; the vault never interprets it.
/// - `key` is the field within that scope, e.g. `api_key`, `password`,
///   `refresh_token`.
///
/// Implementations MUST keep secret values out of any error returned from
/// these methods (logs are scrubbed by callers, not by the vault). The trait
/// deliberately offers no "list all secrets" — `list` is scope-bounded so
/// callers can't accidentally enumerate every credential.
#[async_trait]
pub trait Vault: Send + Sync {
    /// Store or replace a secret. Empty values are valid (a deliberate "this
    /// field is intentionally blank" marker); call `delete` to remove.
    async fn set(&self, scope: &str, key: &str, value: &str) -> Result<()>;

    /// Retrieve a secret. Returns `None` when the entry doesn't exist.
    async fn get(&self, scope: &str, key: &str) -> Result<Option<String>>;

    /// Remove a secret. No-op when the entry doesn't exist (idempotent
    /// — callers shouldn't have to check first).
    async fn delete(&self, scope: &str, key: &str) -> Result<()>;

    /// List the keys present in a single scope. Values are never returned.
    /// Order is unspecified; callers that need stable order must sort.
    async fn list(&self, scope: &str) -> Result<Vec<String>>;
}
