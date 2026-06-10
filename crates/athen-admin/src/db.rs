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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;

    fn temp_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("panel.db")).unwrap();
        (dir, db)
    }

    #[test]
    fn random_token_is_64_hex() {
        let t = random_token();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(t, random_token());
    }

    #[tokio::test]
    async fn user_roundtrip_and_password_verify() {
        let (_dir, db) = temp_db();
        let created = auth::create_user(&db, "alice", "s3cret-pw", "user")
            .await
            .unwrap();
        assert!(!created.is_admin());
        let found = auth::user_by_name(&db, "alice").await.unwrap().unwrap();
        assert_eq!(found.id, created.id);
        assert!(auth::verify_password("s3cret-pw".into(), found.password_hash.clone()).await);
        assert!(!auth::verify_password("wrong".into(), found.password_hash).await);
        assert!(auth::user_by_name(&db, "nobody").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn session_create_resolve_delete() {
        let (_dir, db) = temp_db();
        let user = auth::create_user(&db, "bob", "password1", "admin")
            .await
            .unwrap();
        let sid = auth::new_session(&db, &user.id).await.unwrap();
        let resolved = auth::user_for_session(&db, &sid).await.unwrap().unwrap();
        assert_eq!(resolved.id, user.id);
        auth::delete_session(&db, &sid).await.unwrap();
        assert!(auth::user_for_session(&db, &sid).await.unwrap().is_none());
        assert!(auth::user_for_session(&db, "bogus")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn grants_gate_non_admins_only() {
        let (_dir, db) = temp_db();
        let admin = auth::create_user(&db, "root", "password1", "admin")
            .await
            .unwrap();
        let user = auth::create_user(&db, "carol", "password1", "user")
            .await
            .unwrap();
        db.call(|c| {
            c.execute(
                "INSERT INTO instances (id, name, container_name, volume_name, http_token, internal_url, created_at) \
                 VALUES ('i1','x','athen-x','athen-x-data','tok','http://athen-x:8787','now')",
                [],
            )
        })
        .await
        .unwrap();
        assert!(auth::user_can_access(&db, &admin, "i1").await.unwrap());
        assert!(!auth::user_can_access(&db, &user, "i1").await.unwrap());
        crate::instances::set_grants(&db, "i1", &[user.id.clone()])
            .await
            .unwrap();
        assert!(auth::user_can_access(&db, &user, "i1").await.unwrap());
        crate::instances::set_grants(&db, "i1", &[]).await.unwrap();
        assert!(!auth::user_can_access(&db, &user, "i1").await.unwrap());
    }

    #[test]
    fn session_cookie_parsing() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("foo=bar; {}=abc123; x=y", auth::SESSION_COOKIE)
                .parse()
                .unwrap(),
        );
        assert_eq!(
            auth::session_cookie_value(&headers).as_deref(),
            Some("abc123")
        );
        headers.clear();
        assert!(auth::session_cookie_value(&headers).is_none());
    }
}
