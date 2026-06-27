//! Registered HTTP endpoint types.
//!
//! Backs the generic `http_request` agent tool. One ship of `http_request`
//! plus this store unlocks ~15 cloud APIs because the user can register any
//! REST-shaped service by URL + auth method, then the agent reaches it
//! through a single tool. Mirrors the Identity / Contacts pattern: all
//! metadata lives in SQLite, secrets live in the vault under
//! `endpoint:<id>` so a credential never round-trips through `config.toml`.
//!
//! See `docs/CLOUD_APIS.md` for the architecture write-up and the 15-preset
//! library that ships out of the box.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Risk classification override for an endpoint. Used when the
/// per-method default ("GET no auth = Low, POST/PUT/DELETE = High") doesn't
/// fit — e.g. a read-only API that returns highly sensitive PII should be
/// `High` even though it's a GET.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum EndpointRisk {
    Low,
    Medium,
    High,
}

/// How the endpoint authenticates each request. The credential value is a
/// pointer to the vault key, NEVER the secret itself — store implementations
/// reject any [`AuthMethod`] that smuggles plaintext.
///
/// All variants except `None` resolve at call time by reading
/// `vault.get("endpoint:<endpoint_id>", <key_for_variant>)`. The variant
/// records the *shape* of the auth (which header / which query param /
/// basic-auth user) so the executor knows where to inject the secret.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AuthMethod {
    /// Public endpoint. No credential ever read.
    None,
    /// `Authorization: Bearer <vault.get(endpoint:<id>, "token")>`.
    BearerToken,
    /// Custom header. Header value is sourced from the vault verbatim. The
    /// header name is fixed at registration (e.g. `X-Api-Key`, `xi-api-key`,
    /// `X-Subscription-Token`). Use `HeaderPrefixed` when the API wants a
    /// fixed token-style prefix (`Token …`, `DeepL-Auth-Key …`) so the user
    /// pastes only the raw key.
    Header { name: String },
    /// Custom header where the value is composed as `<prefix><vault.get(..,"value")>`.
    /// Used by APIs like Deepgram (`Authorization: Token <key>`) and DeepL
    /// (`Authorization: DeepL-Auth-Key <key>`) so the user pastes the raw
    /// key and the dispatcher adds the prefix. `prefix` is part of the row
    /// (never secret) and persists across edits.
    HeaderPrefixed { name: String, prefix: String },
    /// Query parameter, value sourced from the vault. Used by SerpAPI,
    /// Hunter, Crawlbase, NewsAPI, OpenCage, etc. The param name is fixed
    /// at registration (e.g. `api_key`, `apiKey`, `key`, `token`).
    QueryParam { name: String },
    /// `Authorization: Basic base64(<user>:<vault.get(endpoint:<id>, "password")>)`.
    /// User is stored in the metadata; password lives in the vault.
    BasicAuth { user: String },
}

impl AuthMethod {
    /// Vault key used by this auth method, if any. Used by
    /// `delete_endpoint` to clean up the vault entry alongside the row.
    pub fn vault_key(&self) -> Option<&'static str> {
        match self {
            AuthMethod::None => None,
            AuthMethod::BearerToken => Some("token"),
            AuthMethod::Header { .. } => Some("value"),
            AuthMethod::HeaderPrefixed { .. } => Some("value"),
            AuthMethod::QueryParam { .. } => Some("value"),
            AuthMethod::BasicAuth { .. } => Some("password"),
        }
    }
}

/// Naive sliding-window cap; counted in-memory per endpoint.
///
/// `requests_per_minute = 0` means "no limit". Stored as a struct (rather
/// than `Option<u32>`) so future fields (burst, cooldown) can land additively
/// without breaking on-disk representation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RateLimit {
    pub requests_per_minute: u32,
}

/// One registered HTTP endpoint. PK is `id`; `name` is the
/// case-insensitive lookup key used by the agent (`http_request` calls
/// reference endpoints by name, not UUID).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RegisteredEndpoint {
    pub id: Uuid,
    /// Display name and case-insensitive lookup key. Unique per install.
    pub name: String,
    /// Human-readable provider label, shown in the UI ("Jina Reader",
    /// "Firecrawl"). May differ from `name` when the user has registered
    /// two endpoints against the same provider.
    pub provider: String,
    /// Base URL — `http_request` joins the per-call `path` onto this.
    /// Stored as `String` (not `Url`) so a misconfigured value can be
    /// loaded and surfaced in the UI rather than crashing the load.
    pub base_url: String,
    pub enabled: bool,
    pub auth_method: AuthMethod,
    /// Sent on every call before per-call header overrides apply.
    pub default_headers: Vec<(String, String)>,
    /// Sent on every call before per-call query overrides apply.
    pub default_query_params: Vec<(String, String)>,
    pub rate_limit: Option<RateLimit>,
    /// When set, overrides the per-method default risk derivation.
    pub risk_override: Option<EndpointRisk>,
    pub notes: Option<String>,
    pub last_used: Option<DateTime<Utc>>,
    /// Rolling 30-day call counter, refreshed lazily from a journal table
    /// in v0 we just bump it on each call and let the UI display the raw
    /// value. Future versions can compute "calls in last 30d" from a
    /// journal if abuse pricing matters.
    pub call_count_30d: u32,
    pub created_at: DateTime<Utc>,
    /// Cached provider logo as a `data:` URL. The provider's favicon is
    /// fetched once (on save / lazy backfill) and stored locally so the UI
    /// never re-fetches over the network. `None` until fetched, or when the
    /// domain has no usable icon — the UI then falls back to a category
    /// glyph. Carried here (not a separate table) since it's small and read
    /// holistically with the rest of the endpoint.
    #[serde(default)]
    pub icon: Option<String>,
}
