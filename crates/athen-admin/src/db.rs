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
        migrate(&conn)?;
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
CREATE TABLE IF NOT EXISTS audit_log (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    at       TEXT NOT NULL,
    username TEXT NOT NULL,
    action   TEXT NOT NULL,
    target   TEXT NOT NULL DEFAULT '',
    detail   TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_audit_at ON audit_log (at DESC);
"#;

/// Column additions for DBs created before the column existed. `ALTER
/// TABLE ADD COLUMN` has no IF NOT EXISTS in SQLite, so "duplicate column
/// name" errors are the idempotency mechanism — anything else propagates.
fn migrate(conn: &Connection) -> anyhow::Result<()> {
    const COLUMNS: &[&str] = &[
        // Per-user push webhook (ntfy topic URL or any plain-POST sink).
        "ALTER TABLE users ADD COLUMN notify_url TEXT NOT NULL DEFAULT ''",
        // Resource quotas applied at container create.
        "ALTER TABLE instances ADD COLUMN memory_mb INTEGER",
        "ALTER TABLE instances ADD COLUMN cpus REAL",
        // Soft disk quota (volume usage warning threshold). Soft because
        // hard Docker storage quotas (`storage_opt`) only work on
        // xfs+pquota backing filesystems — see docker.rs.
        "ALTER TABLE instances ADD COLUMN disk_limit_mb INTEGER",
    ];
    for stmt in COLUMNS {
        match conn.execute(stmt, []) {
            Ok(_) => {}
            Err(e) if e.to_string().contains("duplicate column name") => {}
            Err(e) => return Err(e).with_context(|| format!("migration failed: {stmt}")),
        }
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct User {
    pub id: String,
    pub username: String,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub role: String,
    pub created_at: String,
    /// Push webhook URL; empty = notifications off for this user.
    pub notify_url: String,
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
            notify_url: row.get("notify_url")?,
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
    /// Container memory limit; `None` = unlimited.
    pub memory_mb: Option<u64>,
    /// Container CPU limit (fractional cores); `None` = unlimited.
    pub cpus: Option<f64>,
    /// Soft disk quota on the data volume; `None` = no warning threshold.
    pub disk_limit_mb: Option<u64>,
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
            memory_mb: row.get("memory_mb")?,
            cpus: row.get("cpus")?,
            disk_limit_mb: row.get("disk_limit_mb")?,
        })
    }
}

/// One audit-trail row. Append-only; admins read it via `GET /panel/audit`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub at: String,
    pub username: String,
    pub action: String,
    pub target: String,
    pub detail: String,
}

/// Record a panel action. Fire-and-forget: an audit write must never fail
/// the operation it describes, so errors are logged and swallowed.
pub async fn audit(db: &Db, username: &str, action: &str, target: &str, detail: &str) {
    let (u, a, t, d) = (
        username.to_string(),
        action.to_string(),
        target.to_string(),
        detail.to_string(),
    );
    let res = db
        .call(move |c| {
            c.execute(
                "INSERT INTO audit_log (at, username, action, target, detail) VALUES (?1,?2,?3,?4,?5)",
                rusqlite::params![chrono::Utc::now().to_rfc3339(), u, a, t, d],
            )
        })
        .await;
    if let Err(e) = res {
        tracing::error!(error = %e, action, "audit write failed");
    }
}

/// Delete audit rows older than `cutoff` (RFC 3339 UTC — lexicographic
/// comparison is chronological for this format). Returns rows deleted.
pub async fn audit_prune_before(db: &Db, cutoff: String) -> anyhow::Result<usize> {
    db.call(move |c| c.execute("DELETE FROM audit_log WHERE at < ?1", [cutoff]))
        .await
}

/// Most recent audit rows, newest first.
pub async fn audit_recent(db: &Db, limit: u32) -> anyhow::Result<Vec<AuditEntry>> {
    db.call(move |c| {
        let mut stmt = c.prepare(
            "SELECT id, at, username, action, target, detail FROM audit_log \
             ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
            Ok(AuditEntry {
                id: r.get(0)?,
                at: r.get(1)?,
                username: r.get(2)?,
                action: r.get(3)?,
                target: r.get(4)?,
                detail: r.get(5)?,
            })
        })?;
        rows.collect()
    })
    .await
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
        crate::instances::set_grants(&db, "i1", std::slice::from_ref(&user.id))
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
