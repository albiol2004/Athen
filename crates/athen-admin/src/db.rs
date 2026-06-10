//! Panel database: users, sessions, instances, user↔instance grants.
//!
//! rusqlite behind a `std::sync::Mutex`, every call dispatched through
//! `spawn_blocking` (same pattern as athen-checkpoint). Panel traffic is
//! tiny; a single connection is plenty and keeps WAL handling trivial.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use rusqlite::Connection;

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db at {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    /// Run a closure against the connection on the blocking pool.
    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let db = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let conn = db.lock().expect("panel db mutex poisoned");
            f(&conn)
        })
        .await
        .context("panel db task join")?
        .map_err(Into::into)
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id            TEXT PRIMARY KEY,
    username      TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    role          TEXT NOT NULL CHECK (role IN ('admin','user')),
    created_at    TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS instances (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL UNIQUE,
    container_name TEXT NOT NULL UNIQUE,
    volume_name    TEXT NOT NULL,
    http_token     TEXT NOT NULL,
    internal_url   TEXT NOT NULL,
    created_at     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS user_instances (
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    instance_id TEXT NOT NULL REFERENCES instances(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, instance_id)
);
"#;

#[derive(Debug, Clone, serde::Serialize)]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: String,
    pub created_at: String,
}

impl User {
    pub fn is_admin(&self) -> bool {
        self.role == "admin"
    }

    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            username: row.get("username")?,
            password_hash: row.get("password_hash")?,
            role: row.get("role")?,
            created_at: row.get("created_at")?,
        })
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Instance {
    pub id: String,
    pub name: String,
    pub container_name: String,
    pub volume_name: String,
    #[serde(skip_serializing)]
    pub http_token: String,
    pub internal_url: String,
    pub created_at: String,
}

impl Instance {
    pub fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            container_name: row.get("container_name")?,
            volume_name: row.get("volume_name")?,
            http_token: row.get("http_token")?,
            internal_url: row.get("internal_url")?,
            created_at: row.get("created_at")?,
        })
    }
}

/// 64 hex chars of OS randomness (two UUIDv4s) — same construction as the
/// instance http_token generator in athen-app. Used for session ids and
/// freshly minted instance tokens.
pub fn random_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
