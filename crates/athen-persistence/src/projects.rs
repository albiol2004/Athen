//! Project persistence.
//!
//! A "Project" is the ChatGPT/Claude-style container that groups many arcs
//! around common work (see `docs/PROJECTS.md`). This store is the pure SQLite
//! persistence layer — DB rows only. All filesystem / workspace-folder logic
//! (creating `Projects/<slug>/`, renaming folders on slug change, etc.) lives
//! in the app layer; this store never touches disk beyond the database.
//!
//! Two tables:
//! - `projects` — one row per project (id, name, derived `folder_slug`,
//!   optional instructions, optional maintained summary).
//! - `project_arc_folds` — per-(project, arc) watermark tracking the largest
//!   `arc_entries.id` already folded into the project summary, for the
//!   incremental hierarchical compaction described in `docs/PROJECTS.md`.

use std::sync::Arc;

use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_core::error::{AthenError, Result};

const PROJECT_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    folder_slug TEXT NOT NULL UNIQUE,
    instructions TEXT,
    summary TEXT,
    summary_updated_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS project_arc_folds (
    project_id TEXT NOT NULL,
    arc_id TEXT NOT NULL,
    last_folded_entry_id INTEGER NOT NULL,
    PRIMARY KEY (project_id, arc_id)
);
"#;

/// A Project: a context-scope above arcs that groups many arcs around common
/// work and holds shared context (instructions + a maintained summary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    /// Filesystem-safe slug derived from `name`, unique across projects.
    /// The app layer maps this to a `Projects/<folder_slug>/` directory.
    pub folder_slug: String,
    /// User/agent-authored standing instructions injected into every member
    /// arc's prompt.
    pub instructions: Option<String>,
    /// Maintained project-wide summary (incremental hierarchical compaction).
    pub summary: Option<String>,
    /// RFC3339 timestamp of the last `summary` write.
    pub summary_updated_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Derive a filesystem-safe slug from a project name: lowercase, runs of
/// non-alphanumeric characters collapse to a single `-`, leading/trailing
/// dashes trimmed. Falls back to `"project"` when the result would be empty.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "project".to_string()
    } else {
        trimmed.to_string()
    }
}

/// SQLite-backed Project storage. Cheap to clone — wraps a shared connection.
#[derive(Clone)]
pub struct ProjectStore {
    conn: Arc<Mutex<Connection>>,
}

/// Map a `projects` row to a [`Project`]. Column order must match the
/// `PROJECT_COLS` SELECT list.
const PROJECT_COLS: &str =
    "id, name, folder_slug, instructions, summary, summary_updated_at, created_at, updated_at";

fn read_project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        name: row.get(1)?,
        folder_slug: row.get(2)?,
        instructions: row.get(3)?,
        summary: row.get(4)?,
        summary_updated_at: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

impl ProjectStore {
    /// Create a new `ProjectStore` wrapping the given connection.
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Create the `projects` and `project_arc_folds` tables if they do not
    /// exist.
    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(PROJECT_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Failed to init project schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Create a new project. Derives a unique `folder_slug` from `name`
    /// (appending `-2`, `-3`… on collision) and returns the created row.
    pub async fn create_project(
        &self,
        name: &str,
        instructions: Option<&str>,
    ) -> Result<Project> {
        let conn = self.conn.clone();
        let id = Uuid::new_v4().to_string();
        let name = name.to_string();
        let instructions = instructions.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            let folder_slug = unique_slug(&conn, &name, None)?;
            conn.execute(
                "INSERT INTO projects \
                 (id, name, folder_slug, instructions, summary, summary_updated_at, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?5)",
                params![id, name, folder_slug, instructions, now],
            )
            .map_err(|e| AthenError::Other(format!("Create project: {e}")))?;
            Ok(Project {
                id,
                name,
                folder_slug,
                instructions,
                summary: None,
                summary_updated_at: None,
                created_at: now.clone(),
                updated_at: now,
            })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Get a single project by id.
    pub async fn get_project(&self, id: &str) -> Result<Option<Project>> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare get_project: {e}")))?;
            stmt.query_row(params![id], read_project_row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!("Query get_project: {other}"))),
                })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List all projects, most recently updated first.
    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {PROJECT_COLS} FROM projects ORDER BY updated_at DESC");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list_projects: {e}")))?;
            let rows = stmt
                .query_map([], read_project_row)
                .map_err(|e| AthenError::Other(format!("Query list_projects: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("Project row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Update a project's name and/or instructions.
    ///
    /// - `name: Some(_)` renames the project and recomputes a fresh unique
    ///   `folder_slug` (excluding this row from the collision check).
    /// - `instructions: Some(opt)` sets the instructions to `opt` (where
    ///   `opt == None` clears them); `instructions: None` leaves them
    ///   untouched.
    ///
    /// `updated_at` is always bumped. Returns the post-update row including the
    /// possibly-new `folder_slug` so the caller can rename the folder on disk.
    pub async fn update_project(
        &self,
        id: &str,
        name: Option<&str>,
        instructions: Option<Option<&str>>,
    ) -> Result<Project> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let name = name.map(|s| s.to_string());
        let instructions = instructions.map(|opt| opt.map(|s| s.to_string()));
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();

            // Load current row so we can return the merged result and so a
            // rename can recompute the slug from the new name.
            let sql = format!("SELECT {PROJECT_COLS} FROM projects WHERE id = ?1");
            let current: Project = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare update_project load: {e}")))?
                .query_row(params![id], read_project_row)
                .map_err(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => {
                        AthenError::Other(format!("Project not found: {id}"))
                    }
                    other => AthenError::Other(format!("Load project for update: {other}")),
                })?;

            let new_name = name.clone().unwrap_or_else(|| current.name.clone());
            let new_slug = match &name {
                Some(n) => unique_slug(&conn, n, Some(&id))?,
                None => current.folder_slug.clone(),
            };
            let new_instructions = match &instructions {
                Some(opt) => opt.clone(),
                None => current.instructions.clone(),
            };

            conn.execute(
                "UPDATE projects \
                 SET name = ?1, folder_slug = ?2, instructions = ?3, updated_at = ?4 \
                 WHERE id = ?5",
                params![new_name, new_slug, new_instructions, now, id],
            )
            .map_err(|e| AthenError::Other(format!("Update project: {e}")))?;

            Ok(Project {
                id,
                name: new_name,
                folder_slug: new_slug,
                instructions: new_instructions,
                summary: current.summary,
                summary_updated_at: current.summary_updated_at,
                created_at: current.created_at,
                updated_at: now,
            })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Delete a project row and its `project_arc_folds` rows.
    ///
    /// Does NOT cascade-delete member arcs — they simply keep a now-dangling
    /// `project_id`; the caller may null those out separately
    /// (`ArcStore` owns that).
    pub async fn delete_project(&self, id: &str) -> Result<()> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "DELETE FROM project_arc_folds WHERE project_id = ?1",
                params![id],
            )
            .map_err(|e| AthenError::Other(format!("Delete project folds: {e}")))?;
            conn.execute("DELETE FROM projects WHERE id = ?1", params![id])
                .map_err(|e| AthenError::Other(format!("Delete project: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// List the arc ids that belong to this project, most recently updated
    /// first. Queries the `arcs` table directly (the `project_id` column owned
    /// by `ArcStore`'s schema) to avoid a dependency on `ArcStore` types.
    pub async fn member_arcs(&self, project_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.clone();
        let project_id = project_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare("SELECT id FROM arcs WHERE project_id = ?1 ORDER BY updated_at DESC")
                .map_err(|e| AthenError::Other(format!("Prepare member_arcs: {e}")))?;
            let rows = stmt
                .query_map(params![project_id], |row| row.get::<_, String>(0))
                .map_err(|e| AthenError::Other(format!("Query member_arcs: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("member_arcs row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Set the project's maintained summary and stamp `summary_updated_at`.
    pub async fn set_summary(&self, project_id: &str, summary: &str) -> Result<()> {
        let conn = self.conn.clone();
        let project_id = project_id.to_string();
        let summary = summary.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE projects SET summary = ?1, summary_updated_at = ?2, updated_at = ?2 \
                 WHERE id = ?3",
                params![summary, now, project_id],
            )
            .map_err(|e| AthenError::Other(format!("Set project summary: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Read the fold watermark (largest folded `arc_entries.id`) for an arc
    /// within a project. `None` means the arc has never been folded.
    pub async fn get_fold_watermark(
        &self,
        project_id: &str,
        arc_id: &str,
    ) -> Result<Option<i64>> {
        let conn = self.conn.clone();
        let project_id = project_id.to_string();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.query_row(
                "SELECT last_folded_entry_id FROM project_arc_folds \
                 WHERE project_id = ?1 AND arc_id = ?2",
                params![project_id, arc_id],
                |row| row.get::<_, i64>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(AthenError::Other(format!("Get fold watermark: {other}"))),
            })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }

    /// Upsert the fold watermark for an arc within a project.
    pub async fn set_fold_watermark(
        &self,
        project_id: &str,
        arc_id: &str,
        last_folded_entry_id: i64,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let project_id = project_id.to_string();
        let arc_id = arc_id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO project_arc_folds (project_id, arc_id, last_folded_entry_id) \
                 VALUES (?1, ?2, ?3) \
                 ON CONFLICT(project_id, arc_id) \
                 DO UPDATE SET last_folded_entry_id = excluded.last_folded_entry_id",
                params![project_id, arc_id, last_folded_entry_id],
            )
            .map_err(|e| AthenError::Other(format!("Set fold watermark: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking error: {e}")))?
    }
}

/// Compute a `folder_slug` for `name` that is unique across the `projects`
/// table, appending `-2`, `-3`… on collision. `exclude_id`, when set, lets a
/// rename keep its own row's current slug from counting as a collision.
///
/// Runs synchronously under an already-held connection lock (callers are
/// inside a `spawn_blocking` closure).
fn unique_slug(
    conn: &Connection,
    name: &str,
    exclude_id: Option<&str>,
) -> Result<String> {
    let base = slugify(name);
    let mut candidate = base.clone();
    let mut n: u32 = 1;
    loop {
        let taken: bool = match exclude_id {
            Some(id) => conn
                .query_row(
                    "SELECT 1 FROM projects WHERE folder_slug = ?1 AND id != ?2 LIMIT 1",
                    params![candidate, id],
                    |_| Ok(()),
                )
                .map(|_| true)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(false),
                    other => Err(AthenError::Other(format!("Slug collision check: {other}"))),
                })?,
            None => conn
                .query_row(
                    "SELECT 1 FROM projects WHERE folder_slug = ?1 LIMIT 1",
                    params![candidate],
                    |_| Ok(()),
                )
                .map(|_| true)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(false),
                    other => Err(AthenError::Other(format!("Slug collision check: {other}"))),
                })?,
        };
        if !taken {
            return Ok(candidate);
        }
        n += 1;
        candidate = format!("{base}-{n}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_store() -> ProjectStore {
        let conn = Connection::open_in_memory().unwrap();
        // The `member_arcs` query reads the `arcs` table (owned by ArcStore's
        // schema). Create a minimal stand-in so the query compiles/runs in
        // isolation without depending on ArcStore.
        conn.execute_batch(
            "CREATE TABLE arcs (id TEXT PRIMARY KEY, project_id TEXT, updated_at TEXT NOT NULL);",
        )
        .unwrap();
        let store = ProjectStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.unwrap();
        store
    }

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("My Project"), "my-project");
        assert_eq!(slugify("  Hello, World!  "), "hello-world");
        assert_eq!(slugify("a---b"), "a-b");
        assert_eq!(slugify("Résumé 2024"), "r-sum-2024");
        assert_eq!(slugify("!!!"), "project");
        assert_eq!(slugify(""), "project");
    }

    #[tokio::test]
    async fn create_get_list() {
        let store = setup_store().await;
        let p = store
            .create_project("Website Redesign", Some("Keep the brand voice."))
            .await
            .unwrap();
        assert_eq!(p.name, "Website Redesign");
        assert_eq!(p.folder_slug, "website-redesign");
        assert_eq!(p.instructions.as_deref(), Some("Keep the brand voice."));
        assert!(p.summary.is_none());

        let got = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(got.id, p.id);
        assert_eq!(got.folder_slug, "website-redesign");

        assert!(store.get_project("nope").await.unwrap().is_none());

        let all = store.list_projects().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, p.id);
    }

    #[tokio::test]
    async fn slug_collision_yields_suffix() {
        let store = setup_store().await;
        let a = store.create_project("Launch", None).await.unwrap();
        let b = store.create_project("Launch", None).await.unwrap();
        let c = store.create_project("launch!", None).await.unwrap();
        assert_eq!(a.folder_slug, "launch");
        assert_eq!(b.folder_slug, "launch-2");
        assert_eq!(c.folder_slug, "launch-3");
    }

    #[tokio::test]
    async fn update_rename_recomputes_slug() {
        let store = setup_store().await;
        let p = store.create_project("Old Name", None).await.unwrap();
        assert_eq!(p.folder_slug, "old-name");

        let updated = store
            .update_project(&p.id, Some("Brand New Name"), None)
            .await
            .unwrap();
        assert_eq!(updated.name, "Brand New Name");
        assert_eq!(updated.folder_slug, "brand-new-name");
        // Instructions untouched (was None).
        assert!(updated.instructions.is_none());

        // Renaming back to a name that slugifies to its own current slug must
        // not append a suffix (exclude_id excludes this row).
        let again = store
            .update_project(&p.id, Some("Brand New Name"), None)
            .await
            .unwrap();
        assert_eq!(again.folder_slug, "brand-new-name");
    }

    #[tokio::test]
    async fn update_instructions_set_and_clear() {
        let store = setup_store().await;
        let p = store.create_project("P", Some("orig")).await.unwrap();

        // Set new instructions, leave name alone.
        let u = store
            .update_project(&p.id, None, Some(Some("new instr")))
            .await
            .unwrap();
        assert_eq!(u.instructions.as_deref(), Some("new instr"));
        assert_eq!(u.name, "P");
        assert_eq!(u.folder_slug, "p");

        // Clear instructions.
        let u2 = store
            .update_project(&p.id, None, Some(None))
            .await
            .unwrap();
        assert!(u2.instructions.is_none());

        // None leaves them untouched (still None after the clear).
        let u3 = store.update_project(&p.id, None, None).await.unwrap();
        assert!(u3.instructions.is_none());
    }

    #[tokio::test]
    async fn summary_set_and_get() {
        let store = setup_store().await;
        let p = store.create_project("P", None).await.unwrap();
        assert!(p.summary.is_none());

        store.set_summary(&p.id, "the rolling summary").await.unwrap();
        let got = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(got.summary.as_deref(), Some("the rolling summary"));
        assert!(got.summary_updated_at.is_some());
    }

    #[tokio::test]
    async fn watermark_set_get_upsert() {
        let store = setup_store().await;
        let p = store.create_project("P", None).await.unwrap();

        // Absent initially.
        assert!(store
            .get_fold_watermark(&p.id, "arc-1")
            .await
            .unwrap()
            .is_none());

        store.set_fold_watermark(&p.id, "arc-1", 42).await.unwrap();
        assert_eq!(
            store.get_fold_watermark(&p.id, "arc-1").await.unwrap(),
            Some(42)
        );

        // Upsert advances the same (project, arc) row.
        store.set_fold_watermark(&p.id, "arc-1", 99).await.unwrap();
        assert_eq!(
            store.get_fold_watermark(&p.id, "arc-1").await.unwrap(),
            Some(99)
        );

        // Distinct arc tracked independently.
        store.set_fold_watermark(&p.id, "arc-2", 7).await.unwrap();
        assert_eq!(
            store.get_fold_watermark(&p.id, "arc-2").await.unwrap(),
            Some(7)
        );
        assert_eq!(
            store.get_fold_watermark(&p.id, "arc-1").await.unwrap(),
            Some(99)
        );
    }

    #[tokio::test]
    async fn delete_removes_project_and_folds() {
        let store = setup_store().await;
        let p = store.create_project("P", None).await.unwrap();
        store.set_fold_watermark(&p.id, "arc-1", 10).await.unwrap();

        store.delete_project(&p.id).await.unwrap();
        assert!(store.get_project(&p.id).await.unwrap().is_none());
        assert!(store
            .get_fold_watermark(&p.id, "arc-1")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn member_arcs_orders_by_updated_at_desc() {
        let store = setup_store().await;
        let p = store.create_project("P", None).await.unwrap();
        let conn = store.conn.clone();
        let pid = p.id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO arcs (id, project_id, updated_at) VALUES ('a1', ?1, '2026-01-01T00:00:00Z')",
                params![pid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO arcs (id, project_id, updated_at) VALUES ('a2', ?1, '2026-02-01T00:00:00Z')",
                params![pid],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO arcs (id, project_id, updated_at) VALUES ('a3', NULL, '2026-03-01T00:00:00Z')",
                [],
            )
            .unwrap();
        })
        .await
        .unwrap();
        let members = store.member_arcs(&p.id).await.unwrap();
        // a2 (Feb) before a1 (Jan); a3 excluded (no project_id).
        assert_eq!(members, vec!["a2".to_string(), "a1".to_string()]);
    }
}
