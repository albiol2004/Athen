//! Per-arc and global directory grants.
//!
//! Grants give an arc (or all arcs, if global) read or write access to a
//! specific directory subtree. System paths are never grantable for write —
//! enforced inside `grant_arc` / `grant_global` as defense in depth.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Access {
    Read = 0,
    Write = 1,
}

impl Access {
    fn from_i64(v: i64) -> Result<Self> {
        match v {
            0 => Ok(Access::Read),
            1 => Ok(Access::Write),
            other => Err(AthenError::Other(format!(
                "Invalid access value in DB: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum GrantScope {
    Arc(Uuid),
    Global,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryGrant {
    pub id: i64,
    pub scope: GrantScope,
    pub path: PathBuf,
    pub access: Access,
    pub granted_at: DateTime<Utc>,
}

/// SQLite-backed directory grant store.
pub struct GrantStore {
    conn: Arc<Mutex<Connection>>,
}

impl GrantStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the grant tables if they do not exist.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(GRANTS_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Failed to init grants schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    fn validate_grant(path: &Path, access: Access) -> Result<PathBuf> {
        let canonical = paths::canonicalize_loose(path);
        if access == Access::Write && paths::is_system_path(&canonical) {
            return Err(AthenError::Other(format!(
                "Refusing to grant write access to system path: {}",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    pub async fn grant_arc(
        &self,
        arc_id: Uuid,
        path: &Path,
        access: Access,
    ) -> Result<DirectoryGrant> {
        let canonical = Self::validate_grant(path, access)?;
        let conn = self.conn.clone();
        let arc_id_str = arc_id.to_string();
        let path_str = canonical.to_string_lossy().to_string();
        let now = Utc::now();
        let now_rfc = now.to_rfc3339();
        let access_int = access as i64;

        tokio::task::spawn_blocking(move || -> Result<i64> {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO arc_directory_grants \
                 (arc_id, path, access, granted_at) VALUES (?1, ?2, ?3, ?4)",
                params![arc_id_str, path_str, access_int, now_rfc],
            )
            .map_err(|e| AthenError::Other(format!("Insert arc grant: {e}")))?;
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM arc_directory_grants \
                     WHERE arc_id = ?1 AND path = ?2 AND access = ?3",
                    params![arc_id_str, path_str, access_int],
                    |row| row.get(0),
                )
                .map_err(|e| AthenError::Other(format!("Lookup grant id: {e}")))?;
            Ok(id)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
        .map(|id| DirectoryGrant {
            id,
            scope: GrantScope::Arc(arc_id),
            path: canonical,
            access,
            granted_at: now,
        })
    }

    pub async fn revoke_arc(
        &self,
        arc_id: Uuid,
        path: &Path,
        access: Access,
    ) -> Result<bool> {
        let canonical = paths::canonicalize_loose(path);
        let conn = self.conn.clone();
        let arc_id_str = arc_id.to_string();
        let path_str = canonical.to_string_lossy().to_string();
        let access_int = access as i64;

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "DELETE FROM arc_directory_grants \
                     WHERE arc_id = ?1 AND path = ?2 AND access = ?3",
                    params![arc_id_str, path_str, access_int],
                )
                .map_err(|e| AthenError::Other(format!("Revoke arc grant: {e}")))?;
            Ok(n > 0)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Revoke an arc grant by its primary-key id. Returns true if a row
    /// was deleted.
    pub async fn revoke_arc_by_id(&self, id: i64) -> Result<bool> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "DELETE FROM arc_directory_grants WHERE id = ?1",
                    params![id],
                )
                .map_err(|e| AthenError::Other(format!("Revoke arc grant by id: {e}")))?;
            Ok(n > 0)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Revoke a global grant by its primary-key id. Returns true if a row
    /// was deleted.
    pub async fn revoke_global_by_id(&self, id: i64) -> Result<bool> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "DELETE FROM global_directory_grants WHERE id = ?1",
                    params![id],
                )
                .map_err(|e| AthenError::Other(format!("Revoke global grant by id: {e}")))?;
            Ok(n > 0)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    pub async fn list_arc(&self, arc_id: Uuid) -> Result<Vec<DirectoryGrant>> {
        let conn = self.conn.clone();
        let arc_id_str = arc_id.to_string();

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, path, access, granted_at \
                     FROM arc_directory_grants WHERE arc_id = ?1 ORDER BY id ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list arc grants: {e}")))?;
            let rows = stmt
                .query_map(params![arc_id_str], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| AthenError::Other(format!("Query list arc grants: {e}")))?;

            let mut out = Vec::new();
            for row in rows {
                let (id, path, access, granted_at) =
                    row.map_err(|e| AthenError::Other(format!("Arc grant row: {e}")))?;
                out.push(DirectoryGrant {
                    id,
                    scope: GrantScope::Arc(arc_id),
                    path: PathBuf::from(path),
                    access: Access::from_i64(access)?,
                    granted_at: DateTime::parse_from_rfc3339(&granted_at)
                        .map_err(|e| AthenError::Other(format!("Parse granted_at: {e}")))?
                        .with_timezone(&Utc),
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    pub async fn grant_global(&self, path: &Path, access: Access) -> Result<DirectoryGrant> {
        let canonical = Self::validate_grant(path, access)?;
        let conn = self.conn.clone();
        let path_str = canonical.to_string_lossy().to_string();
        let now = Utc::now();
        let now_rfc = now.to_rfc3339();
        let access_int = access as i64;

        tokio::task::spawn_blocking(move || -> Result<i64> {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO global_directory_grants \
                 (path, access, granted_at) VALUES (?1, ?2, ?3)",
                params![path_str, access_int, now_rfc],
            )
            .map_err(|e| AthenError::Other(format!("Insert global grant: {e}")))?;
            let id: i64 = conn
                .query_row(
                    "SELECT id FROM global_directory_grants \
                     WHERE path = ?1 AND access = ?2",
                    params![path_str, access_int],
                    |row| row.get(0),
                )
                .map_err(|e| AthenError::Other(format!("Lookup global grant id: {e}")))?;
            Ok(id)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
        .map(|id| DirectoryGrant {
            id,
            scope: GrantScope::Global,
            path: canonical,
            access,
            granted_at: now,
        })
    }

    pub async fn revoke_global(&self, path: &Path, access: Access) -> Result<bool> {
        let canonical = paths::canonicalize_loose(path);
        let conn = self.conn.clone();
        let path_str = canonical.to_string_lossy().to_string();
        let access_int = access as i64;

        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "DELETE FROM global_directory_grants \
                     WHERE path = ?1 AND access = ?2",
                    params![path_str, access_int],
                )
                .map_err(|e| AthenError::Other(format!("Revoke global grant: {e}")))?;
            Ok(n > 0)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    pub async fn list_global(&self) -> Result<Vec<DirectoryGrant>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, path, access, granted_at \
                     FROM global_directory_grants ORDER BY id ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list global grants: {e}")))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .map_err(|e| AthenError::Other(format!("Query list global grants: {e}")))?;

            let mut out = Vec::new();
            for row in rows {
                let (id, path, access, granted_at) =
                    row.map_err(|e| AthenError::Other(format!("Global grant row: {e}")))?;
                out.push(DirectoryGrant {
                    id,
                    scope: GrantScope::Global,
                    path: PathBuf::from(path),
                    access: Access::from_i64(access)?,
                    granted_at: DateTime::parse_from_rfc3339(&granted_at)
                        .map_err(|e| AthenError::Other(format!("Parse granted_at: {e}")))?
                        .with_timezone(&Utc),
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Look up whether `path` is covered by a grant for the given arc (or
    /// any global grant). Walks parent directories so a grant on `/foo`
    /// covers `/foo/bar/baz`. Write grants satisfy read queries.
    /// Arc grants are checked before global grants.
    pub async fn check(
        &self,
        arc_id: Uuid,
        path: &Path,
        access: Access,
    ) -> Result<Option<GrantScope>> {
        let canonical = paths::canonicalize_loose(path);
        let arc_grants = self.list_arc(arc_id).await?;
        if let Some(g) = find_covering(&arc_grants, &canonical, access) {
            return Ok(Some(g.scope.clone()));
        }
        let global_grants = self.list_global().await?;
        if let Some(g) = find_covering(&global_grants, &canonical, access) {
            return Ok(Some(g.scope.clone()));
        }
        Ok(None)
    }
}

fn find_covering<'a>(
    grants: &'a [DirectoryGrant],
    target: &Path,
    needed: Access,
) -> Option<&'a DirectoryGrant> {
    grants.iter().find(|g| {
        let access_ok = match needed {
            Access::Read => true,
            Access::Write => g.access == Access::Write,
        };
        access_ok && paths::path_within(target, &g.path)
    })
}

const GRANTS_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS arc_directory_grants (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    arc_id TEXT NOT NULL,
    path TEXT NOT NULL,
    access INTEGER NOT NULL,
    granted_at TEXT NOT NULL,
    UNIQUE(arc_id, path, access)
);
CREATE INDEX IF NOT EXISTS idx_arc_grants_arc ON arc_directory_grants(arc_id);

CREATE TABLE IF NOT EXISTS global_directory_grants (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL,
    access INTEGER NOT NULL,
    granted_at TEXT NOT NULL,
    UNIQUE(path, access)
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> GrantStore {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = GrantStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init grants schema");
        store
    }

    #[tokio::test]
    async fn grant_arc_then_check() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        let dir = std::env::temp_dir().join(format!("athen_grant_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        store.grant_arc(arc, &dir, Access::Write).await.unwrap();
        let scope = store.check(arc, &dir, Access::Write).await.unwrap();
        assert!(matches!(scope, Some(GrantScope::Arc(a)) if a == arc));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn grant_arc_covers_descendants() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        let parent = std::env::temp_dir().join(format!("athen_grant_parent_{}", Uuid::new_v4()));
        let child = parent.join("nested").join("file.txt");
        std::fs::create_dir_all(&parent).unwrap();

        store.grant_arc(arc, &parent, Access::Write).await.unwrap();
        let scope = store.check(arc, &child, Access::Write).await.unwrap();
        assert!(matches!(scope, Some(GrantScope::Arc(_))));

        let _ = std::fs::remove_dir_all(&parent);
    }

    #[tokio::test]
    async fn grant_arc_does_not_leak_to_sibling() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        let bar = Path::new("/foo/bar");
        let baz = Path::new("/foo/baz");

        store.grant_arc(arc, bar, Access::Read).await.unwrap();
        let scope = store.check(arc, baz, Access::Read).await.unwrap();
        assert!(scope.is_none());
    }

    #[tokio::test]
    async fn cannot_grant_write_to_system_path() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        let result = store.grant_arc(arc, Path::new("/etc"), Access::Write).await;
        assert!(result.is_err());

        let result = store.grant_global(Path::new("/etc/passwd"), Access::Write).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn can_grant_read_to_system_path() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        store
            .grant_arc(arc, Path::new("/etc"), Access::Read)
            .await
            .expect("read grants on system paths are allowed");
    }

    #[tokio::test]
    async fn global_grant_visible_from_any_arc() {
        let store = setup().await;
        let arc1 = Uuid::new_v4();
        let arc2 = Uuid::new_v4();
        let dir = Path::new("/tmp/athen_test_global");

        store.grant_global(dir, Access::Read).await.unwrap();
        let scope = store.check(arc1, dir, Access::Read).await.unwrap();
        assert!(matches!(scope, Some(GrantScope::Global)));
        let scope = store.check(arc2, dir, Access::Read).await.unwrap();
        assert!(matches!(scope, Some(GrantScope::Global)));
    }

    #[tokio::test]
    async fn arc_grant_overrides_global() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        let dir = Path::new("/tmp/athen_override_test");

        store.grant_global(dir, Access::Read).await.unwrap();
        store.grant_arc(arc, dir, Access::Read).await.unwrap();

        let scope = store.check(arc, dir, Access::Read).await.unwrap();
        assert!(matches!(scope, Some(GrantScope::Arc(a)) if a == arc));
    }

    #[tokio::test]
    async fn write_grant_satisfies_read_query() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        let dir = Path::new("/tmp/athen_write_satisfies_read");

        store.grant_arc(arc, dir, Access::Write).await.unwrap();
        let scope = store.check(arc, dir, Access::Read).await.unwrap();
        assert!(matches!(scope, Some(GrantScope::Arc(_))));
    }

    #[tokio::test]
    async fn read_grant_does_not_satisfy_write_query() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        let dir = Path::new("/tmp/athen_read_only");

        store.grant_arc(arc, dir, Access::Read).await.unwrap();
        let scope = store.check(arc, dir, Access::Write).await.unwrap();
        assert!(scope.is_none());
    }

    #[tokio::test]
    async fn revoke_removes_grant() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        let dir = Path::new("/tmp/athen_revoke_test");

        store.grant_arc(arc, dir, Access::Read).await.unwrap();
        let removed = store.revoke_arc(arc, dir, Access::Read).await.unwrap();
        assert!(removed);

        let scope = store.check(arc, dir, Access::Read).await.unwrap();
        assert!(scope.is_none());

        let removed_again = store.revoke_arc(arc, dir, Access::Read).await.unwrap();
        assert!(!removed_again);
    }

    #[tokio::test]
    async fn list_arc_returns_all_grants() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        store
            .grant_arc(arc, Path::new("/tmp/athen_list_a"), Access::Read)
            .await
            .unwrap();
        store
            .grant_arc(arc, Path::new("/tmp/athen_list_b"), Access::Write)
            .await
            .unwrap();

        let grants = store.list_arc(arc).await.unwrap();
        assert_eq!(grants.len(), 2);
    }

    #[tokio::test]
    async fn revoke_arc_by_id_removes_row() {
        let store = setup().await;
        let arc = Uuid::new_v4();
        let g = store
            .grant_arc(arc, Path::new("/tmp/athen_revoke_by_id"), Access::Read)
            .await
            .unwrap();
        assert!(store.revoke_arc_by_id(g.id).await.unwrap());
        assert!(!store.revoke_arc_by_id(g.id).await.unwrap());
    }

    #[tokio::test]
    async fn revoke_global_by_id_removes_row() {
        let store = setup().await;
        let g = store
            .grant_global(Path::new("/tmp/athen_g_revoke_by_id"), Access::Read)
            .await
            .unwrap();
        assert!(store.revoke_global_by_id(g.id).await.unwrap());
        assert!(!store.revoke_global_by_id(g.id).await.unwrap());
    }

    #[tokio::test]
    async fn list_global_returns_all_grants() {
        let store = setup().await;
        store
            .grant_global(Path::new("/tmp/athen_glist_a"), Access::Read)
            .await
            .unwrap();
        store
            .grant_global(Path::new("/tmp/athen_glist_b"), Access::Write)
            .await
            .unwrap();
        let grants = store.list_global().await.unwrap();
        assert_eq!(grants.len(), 2);
    }
}
