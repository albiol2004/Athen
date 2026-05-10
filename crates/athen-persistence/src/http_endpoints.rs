//! SQLite-backed `HttpEndpointStore`.
//!
//! One table: `http_endpoints`. `auth_method`, `default_headers`,
//! `default_query_params`, and `rate_limit` are JSON columns — endpoints
//! are read holistically and never queried by their internals, so a
//! relational shape would be over-engineered.
//!
//! `name` is enforced unique via a case-insensitive index so the agent's
//! `http_request endpoint="jina"` matches a row registered as "Jina".

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::http_endpoint::{AuthMethod, EndpointRisk, RateLimit, RegisteredEndpoint};
use athen_core::traits::http_endpoint::HttpEndpointStore;

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS http_endpoints (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    provider TEXT NOT NULL DEFAULT '',
    base_url TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    auth_method_json TEXT NOT NULL,
    default_headers_json TEXT NOT NULL DEFAULT '[]',
    default_query_json TEXT NOT NULL DEFAULT '[]',
    rate_limit_json TEXT,
    risk_override TEXT,
    notes TEXT,
    last_used TEXT,
    call_count_30d INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_http_endpoints_name_ci
    ON http_endpoints(name COLLATE NOCASE);
"#;

const COLS: &str = "id, name, provider, base_url, enabled, auth_method_json, \
default_headers_json, default_query_json, rate_limit_json, risk_override, notes, \
last_used, call_count_30d, created_at";

pub struct SqliteHttpEndpointStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteHttpEndpointStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Init http_endpoints schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

fn risk_to_str(r: EndpointRisk) -> &'static str {
    match r {
        EndpointRisk::Low => "low",
        EndpointRisk::Medium => "medium",
        EndpointRisk::High => "high",
    }
}

fn risk_from_str(s: &str) -> Option<EndpointRisk> {
    match s {
        "low" => Some(EndpointRisk::Low),
        "medium" => Some(EndpointRisk::Medium),
        "high" => Some(EndpointRisk::High),
        _ => None,
    }
}

fn read_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegisteredEndpoint> {
    let id_str: String = row.get(0)?;
    let name: String = row.get(1)?;
    let provider: String = row.get(2)?;
    let base_url: String = row.get(3)?;
    let enabled: i64 = row.get(4)?;
    let auth_method_json: String = row.get(5)?;
    let default_headers_json: String = row.get(6)?;
    let default_query_json: String = row.get(7)?;
    let rate_limit_json: Option<String> = row.get(8)?;
    let risk_override_str: Option<String> = row.get(9)?;
    let notes: Option<String> = row.get(10)?;
    let last_used_str: Option<String> = row.get(11)?;
    let call_count_30d: i64 = row.get(12)?;
    let created_at_str: String = row.get(13)?;

    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let auth_method: AuthMethod = serde_json::from_str(&auth_method_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let default_headers: Vec<(String, String)> = serde_json::from_str(&default_headers_json)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let default_query_params: Vec<(String, String)> = serde_json::from_str(&default_query_json)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?;
    let rate_limit: Option<RateLimit> = match rate_limit_json {
        Some(j) => Some(serde_json::from_str(&j).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?),
        None => None,
    };
    let risk_override = risk_override_str.as_deref().and_then(risk_from_str);
    let last_used = match last_used_str {
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(&s)
                .map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        11,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?
                .with_timezone(&Utc),
        ),
        None => None,
    };
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at_str)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(13, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);

    Ok(RegisteredEndpoint {
        id,
        name,
        provider,
        base_url,
        enabled: enabled != 0,
        auth_method,
        default_headers,
        default_query_params,
        rate_limit,
        risk_override,
        notes,
        last_used,
        call_count_30d: call_count_30d.max(0) as u32,
        created_at,
    })
}

#[async_trait]
impl HttpEndpointStore for SqliteHttpEndpointStore {
    async fn list(&self) -> Result<Vec<RegisteredEndpoint>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {COLS} FROM http_endpoints ORDER BY name COLLATE NOCASE ASC");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list endpoints: {e}")))?;
            let rows = stmt
                .query_map([], read_row)
                .map_err(|e| AthenError::Other(format!("Query endpoints: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("Endpoint row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn get(&self, id: Uuid) -> Result<Option<RegisteredEndpoint>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {COLS} FROM http_endpoints WHERE id = ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare get endpoint: {e}")))?;
            stmt.query_row(params![id.to_string()], read_row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!("Query endpoint: {other}"))),
                })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn get_by_name(&self, name: &str) -> Result<Option<RegisteredEndpoint>> {
        let conn = self.conn.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql =
                format!("SELECT {COLS} FROM http_endpoints WHERE name = ?1 COLLATE NOCASE LIMIT 1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare get_by_name: {e}")))?;
            stmt.query_row(params![name], read_row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!(
                        "Query endpoint by name: {other}"
                    ))),
                })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn upsert(&self, endpoint: &RegisteredEndpoint) -> Result<()> {
        if endpoint.name.trim().is_empty() {
            return Err(AthenError::Other(
                "Endpoint name cannot be empty".to_string(),
            ));
        }
        if endpoint.base_url.trim().is_empty() {
            return Err(AthenError::Other(
                "Endpoint base_url cannot be empty".to_string(),
            ));
        }
        // Preserve created_at on replace.
        let existing = self.get(endpoint.id).await?;
        let mut e = endpoint.clone();
        if let Some(prev) = existing {
            e.created_at = prev.created_at;
            // Don't reset usage counters from the UI path.
            e.last_used = prev.last_used.or(e.last_used);
            e.call_count_30d = prev.call_count_30d.max(e.call_count_30d);
        }
        // Reject a name collision with a different row. INSERT OR REPLACE
        // would silently nuke the conflicting unique-index row otherwise.
        if let Some(by_name) = self.get_by_name(&e.name).await? {
            if by_name.id != e.id {
                return Err(AthenError::Other(format!(
                    "Endpoint name '{}' is already registered (UNIQUE constraint)",
                    e.name
                )));
            }
        }
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let auth_method_json =
                serde_json::to_string(&e.auth_method).map_err(AthenError::Serialization)?;
            let default_headers_json =
                serde_json::to_string(&e.default_headers).map_err(AthenError::Serialization)?;
            let default_query_json = serde_json::to_string(&e.default_query_params)
                .map_err(AthenError::Serialization)?;
            let rate_limit_json = match e.rate_limit {
                Some(rl) => Some(serde_json::to_string(&rl).map_err(AthenError::Serialization)?),
                None => None,
            };
            conn.execute(
                "INSERT OR REPLACE INTO http_endpoints \
                 (id, name, provider, base_url, enabled, auth_method_json, \
                  default_headers_json, default_query_json, rate_limit_json, risk_override, \
                  notes, last_used, call_count_30d, created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    e.id.to_string(),
                    e.name,
                    e.provider,
                    e.base_url,
                    e.enabled as i64,
                    auth_method_json,
                    default_headers_json,
                    default_query_json,
                    rate_limit_json,
                    e.risk_override.map(risk_to_str),
                    e.notes,
                    e.last_used.map(|t| t.to_rfc3339()),
                    e.call_count_30d as i64,
                    e.created_at.to_rfc3339(),
                ],
            )
            .map_err(|err| AthenError::Other(format!("Upsert endpoint: {err}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "DELETE FROM http_endpoints WHERE id = ?1",
                    params![id.to_string()],
                )
                .map_err(|e| AthenError::Other(format!("Delete endpoint: {e}")))?;
            if n == 0 {
                return Err(AthenError::Other(format!("Endpoint not found: {id}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn record_call(&self, id: Uuid) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "UPDATE http_endpoints \
                 SET call_count_30d = call_count_30d + 1, last_used = ?2 \
                 WHERE id = ?1",
                params![id.to_string(), Utc::now().to_rfc3339()],
            )
            .map_err(|e| AthenError::Other(format!("Record endpoint call: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "UPDATE http_endpoints SET enabled = ?2 WHERE id = ?1",
                    params![id.to_string(), enabled as i64],
                )
                .map_err(|e| AthenError::Other(format!("Set endpoint enabled: {e}")))?;
            if n == 0 {
                return Err(AthenError::Other(format!("Endpoint not found: {id}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> SqliteHttpEndpointStore {
        let conn = Connection::open_in_memory().unwrap();
        let store = SqliteHttpEndpointStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.unwrap();
        store
    }

    fn mk_endpoint(name: &str) -> RegisteredEndpoint {
        let now = Utc::now();
        RegisteredEndpoint {
            id: Uuid::new_v4(),
            name: name.into(),
            provider: name.into(),
            base_url: "https://example.com/v1/".into(),
            enabled: true,
            auth_method: AuthMethod::BearerToken,
            default_headers: vec![("Accept".into(), "application/json".into())],
            default_query_params: vec![],
            rate_limit: Some(RateLimit {
                requests_per_minute: 60,
            }),
            risk_override: None,
            notes: None,
            last_used: None,
            call_count_30d: 0,
            created_at: now,
        }
    }

    #[tokio::test]
    async fn upsert_and_lookup_round_trip() {
        let store = setup().await;
        let e = mk_endpoint("Jina");
        store.upsert(&e).await.unwrap();
        let by_id = store.get(e.id).await.unwrap().unwrap();
        assert_eq!(by_id.name, "Jina");
        assert_eq!(by_id.auth_method, AuthMethod::BearerToken);
        assert_eq!(by_id.default_headers.len(), 1);
    }

    #[tokio::test]
    async fn get_by_name_is_case_insensitive() {
        let store = setup().await;
        let e = mk_endpoint("Jina");
        store.upsert(&e).await.unwrap();
        assert!(store.get_by_name("jina").await.unwrap().is_some());
        assert!(store.get_by_name("JINA").await.unwrap().is_some());
        assert!(store.get_by_name("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn duplicate_name_rejected() {
        let store = setup().await;
        store.upsert(&mk_endpoint("Jina")).await.unwrap();
        let dup = mk_endpoint("jina");
        let err = store.upsert(&dup).await.unwrap_err();
        // SQLite unique-index violation surfaces via our error wrapper.
        assert!(err.to_string().to_lowercase().contains("unique"));
    }

    #[tokio::test]
    async fn empty_name_rejected() {
        let store = setup().await;
        let mut e = mk_endpoint("Jina");
        e.name = "   ".into();
        let err = store.upsert(&e).await.unwrap_err();
        assert!(err.to_string().contains("name cannot be empty"));
    }

    #[tokio::test]
    async fn empty_base_url_rejected() {
        let store = setup().await;
        let mut e = mk_endpoint("Jina");
        e.base_url = "".into();
        let err = store.upsert(&e).await.unwrap_err();
        assert!(err.to_string().contains("base_url cannot be empty"));
    }

    #[tokio::test]
    async fn set_enabled_toggles_flag() {
        let store = setup().await;
        let e = mk_endpoint("Jina");
        store.upsert(&e).await.unwrap();
        store.set_enabled(e.id, false).await.unwrap();
        let loaded = store.get(e.id).await.unwrap().unwrap();
        assert!(!loaded.enabled);
    }

    #[tokio::test]
    async fn record_call_bumps_counter_and_stamp() {
        let store = setup().await;
        let e = mk_endpoint("Jina");
        store.upsert(&e).await.unwrap();
        store.record_call(e.id).await.unwrap();
        store.record_call(e.id).await.unwrap();
        let loaded = store.get(e.id).await.unwrap().unwrap();
        assert_eq!(loaded.call_count_30d, 2);
        assert!(loaded.last_used.is_some());
    }

    #[tokio::test]
    async fn delete_missing_errors() {
        let store = setup().await;
        let err = store.delete(Uuid::new_v4()).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn upsert_replace_preserves_created_at_and_counters() {
        let store = setup().await;
        let mut e = mk_endpoint("Jina");
        let original_created = e.created_at;
        store.upsert(&e).await.unwrap();
        store.record_call(e.id).await.unwrap();
        // User edits the endpoint.
        e.notes = Some("trusted".into());
        e.created_at = Utc::now() + chrono::Duration::seconds(60); // attempted reset
        e.call_count_30d = 0; // attempted reset
        store.upsert(&e).await.unwrap();
        let loaded = store.get(e.id).await.unwrap().unwrap();
        assert_eq!(loaded.created_at, original_created);
        assert_eq!(loaded.call_count_30d, 1);
        assert_eq!(loaded.notes.as_deref(), Some("trusted"));
    }

    #[tokio::test]
    async fn list_returns_alpha_sorted() {
        let store = setup().await;
        store.upsert(&mk_endpoint("zebra")).await.unwrap();
        store.upsert(&mk_endpoint("Apple")).await.unwrap();
        store.upsert(&mk_endpoint("mango")).await.unwrap();
        let list = store.list().await.unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Apple", "mango", "zebra"]);
    }
}
