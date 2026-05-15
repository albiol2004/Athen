//! Hybrid `SkillStore`: SQLite index over a filesystem of `SKILL.md` files.
//!
//! Bodies live on disk at `<skills_dir>/<slug>/SKILL.md` (source of truth,
//! human-editable, git-friendly). The SQLite index is *derived state* — a
//! cache that lets the prompt builder list skills cheaply without re-parsing
//! every file on every turn.
//!
//! On boot the composition root calls [`SqliteSkillStore::sync`], which
//! walks the filesystem and reconciles the index (insert new, update changed,
//! delete missing). This makes the filesystem authoritative: a user can
//! `git clone` a skills repo into the directory and Athen picks it up on the
//! next start.
//!
//! See `docs/SKILLS.md` for the full design.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use athen_core::error::{AthenError, Result};
use athen_core::identity::applies_to_profile;
use athen_core::skill::{parse_skill_md, serialize_skill_md, Skill, SkillFrontmatter, SkillSource};
use athen_core::traits::skill::{SkillStore, SyncReport};

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS skills_index (
    slug TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    applies_to_json TEXT NOT NULL,
    source TEXT NOT NULL,
    body_path TEXT NOT NULL,
    hash TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_skills_source ON skills_index(source);
"#;

const SKILL_COLS: &str =
    "slug, name, description, applies_to_json, source, body_path, hash, created_at, updated_at";

/// Initialize the `skills_index` table on the shared connection. Called by
/// `Database::run_migrations` so callers don't need the `skills_dir` just to
/// run migrations.
pub async fn init_schema(conn: &Arc<Mutex<Connection>>) -> Result<()> {
    let conn = conn.clone();
    tokio::task::spawn_blocking(move || {
        let c = conn.blocking_lock();
        c.execute_batch(SCHEMA_SQL)
            .map_err(|e| AthenError::Other(format!("Init skill schema: {e}")))?;
        Ok(())
    })
    .await
    .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
}

pub struct SqliteSkillStore {
    conn: Arc<Mutex<Connection>>,
    skills_dir: PathBuf,
}

impl SqliteSkillStore {
    pub fn new(conn: Arc<Mutex<Connection>>, skills_dir: PathBuf) -> Self {
        Self { conn, skills_dir }
    }

    pub fn skills_dir(&self) -> &Path {
        &self.skills_dir
    }
}

/// Slug shape: ascii alphanumerics plus `-` and `_`. No path-traversal
/// characters, no leading dot (would shadow dotfiles + cause hidden-folder
/// surprises). Errors are user-facing so the Settings UI can surface them.
fn validate_slug(slug: &str) -> Result<()> {
    if slug.is_empty() {
        return Err(AthenError::Other("skill slug cannot be empty".into()));
    }
    if slug.starts_with('.') {
        return Err(AthenError::Other("skill slug cannot start with '.'".into()));
    }
    for ch in slug.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '-' || ch == '_';
        if !ok {
            return Err(AthenError::Other(format!(
                "skill slug '{slug}' contains invalid char '{ch}' (allowed: a-z A-Z 0-9 - _)"
            )));
        }
    }
    Ok(())
}

fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn read_skill_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Skill> {
    let slug: String = row.get(0)?;
    let name: String = row.get(1)?;
    let description: String = row.get(2)?;
    let applies_to_json: String = row.get(3)?;
    let source_s: String = row.get(4)?;
    let body_path: String = row.get(5)?;
    let hash: String = row.get(6)?;
    let created_at: String = row.get(7)?;
    let updated_at: String = row.get(8)?;

    let applies_to = serde_json::from_str(&applies_to_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let source = SkillSource::parse(&source_s).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid source: {source_s}"),
            )),
        )
    })?;
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(7, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);
    let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&Utc);

    Ok(Skill {
        slug,
        name,
        description,
        applies_to,
        source,
        body_path: PathBuf::from(body_path),
        hash,
        created_at,
        updated_at,
    })
}

#[async_trait]
impl SkillStore for SqliteSkillStore {
    async fn list(&self, profile: Option<&str>) -> Result<Vec<Skill>> {
        let conn = self.conn.clone();
        let all: Vec<Skill> = tokio::task::spawn_blocking(move || {
            let c = conn.blocking_lock();
            let sql = format!("SELECT {SKILL_COLS} FROM skills_index ORDER BY slug ASC");
            let mut stmt = c
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list: {e}")))?;
            let rows = stmt
                .query_map([], read_skill_row)
                .map_err(|e| AthenError::Other(format!("Query skills: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("Skill row: {e}")))?);
            }
            Ok::<_, AthenError>(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))??;

        match profile {
            Some(p) => Ok(all
                .into_iter()
                .filter(|s| applies_to_profile(&s.applies_to, p))
                .collect()),
            None => Ok(all),
        }
    }

    async fn get(&self, slug: &str) -> Result<Option<Skill>> {
        let conn = self.conn.clone();
        let slug = slug.to_string();
        tokio::task::spawn_blocking(move || {
            let c = conn.blocking_lock();
            let sql = format!("SELECT {SKILL_COLS} FROM skills_index WHERE slug = ?1");
            let mut stmt = c
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare get: {e}")))?;
            stmt.query_row(params![slug], read_skill_row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!("Query skill: {other}"))),
                })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn load_body(&self, slug: &str) -> Result<String> {
        let skill = self
            .get(slug)
            .await?
            .ok_or_else(|| AthenError::Other(format!("skill not found: {slug}")))?;
        let raw = tokio::fs::read_to_string(&skill.body_path)
            .await
            .map_err(|e| AthenError::Other(format!("Read SKILL.md: {e}")))?;
        let (_, body) = parse_skill_md(&raw)?;
        Ok(body)
    }

    async fn upsert(&self, slug: &str, frontmatter: &SkillFrontmatter, body: &str) -> Result<()> {
        validate_slug(slug)?;
        if frontmatter.name.trim().is_empty() {
            return Err(AthenError::Other("skill name cannot be empty".into()));
        }
        if frontmatter.description.trim().is_empty() {
            return Err(AthenError::Other(
                "skill description cannot be empty".into(),
            ));
        }

        let folder = self.skills_dir.join(slug);
        tokio::fs::create_dir_all(&folder)
            .await
            .map_err(|e| AthenError::Other(format!("Create skill folder: {e}")))?;
        let body_path = folder.join("SKILL.md");
        let serialized = serialize_skill_md(frontmatter, body);
        let hash = compute_hash(&serialized);
        // Write the file BEFORE the index update so a failed write doesn't
        // leave an index row pointing at a missing file. If the index write
        // fails after, the next sync pass repairs by inserting the file as
        // a "new" entry.
        tokio::fs::write(&body_path, &serialized)
            .await
            .map_err(|e| AthenError::Other(format!("Write SKILL.md: {e}")))?;

        let existing = self.get(slug).await?;
        let (created_at, source) = match existing {
            Some(prev) => (prev.created_at, prev.source),
            None => (Utc::now(), SkillSource::User),
        };
        let updated_at = Utc::now();

        self.insert_row(
            slug,
            frontmatter,
            &hash,
            &body_path,
            source,
            created_at,
            updated_at,
        )
        .await
    }

    async fn delete(&self, slug: &str) -> Result<()> {
        validate_slug(slug)?;
        let existing = self.get(slug).await?;
        if existing.is_none() {
            return Err(AthenError::Other(format!("skill not found: {slug}")));
        }
        let folder = self.skills_dir.join(slug);
        if folder.exists() {
            tokio::fs::remove_dir_all(&folder)
                .await
                .map_err(|e| AthenError::Other(format!("Remove skill folder: {e}")))?;
        }
        let conn = self.conn.clone();
        let slug_s = slug.to_string();
        tokio::task::spawn_blocking(move || {
            let c = conn.blocking_lock();
            c.execute("DELETE FROM skills_index WHERE slug = ?1", params![slug_s])
                .map_err(|e| AthenError::Other(format!("Delete skill: {e}")))?;
            Ok::<_, AthenError>(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn sync(&self) -> Result<SyncReport> {
        tokio::fs::create_dir_all(&self.skills_dir)
            .await
            .map_err(|e| AthenError::Other(format!("Create skills dir: {e}")))?;

        // 1. Snapshot the index.
        let conn = self.conn.clone();
        let existing: HashMap<String, (String, SkillSource, DateTime<Utc>)> =
            tokio::task::spawn_blocking(move || {
                let c = conn.blocking_lock();
                let mut stmt = c
                    .prepare("SELECT slug, hash, source, created_at FROM skills_index")
                    .map_err(|e| AthenError::Other(format!("Prepare sync read: {e}")))?;
                let rows = stmt
                    .query_map([], |row| {
                        let slug: String = row.get(0)?;
                        let hash: String = row.get(1)?;
                        let source: String = row.get(2)?;
                        let created: String = row.get(3)?;
                        Ok((slug, hash, source, created))
                    })
                    .map_err(|e| AthenError::Other(format!("Query sync read: {e}")))?;
                let mut map = HashMap::new();
                for r in rows {
                    let (slug, hash, source_s, created_s) =
                        r.map_err(|e| AthenError::Other(format!("Sync row: {e}")))?;
                    let source = SkillSource::parse(&source_s).unwrap_or(SkillSource::User);
                    let created = chrono::DateTime::parse_from_rfc3339(&created_s)
                        .map_err(|e| AthenError::Other(format!("Parse created_at: {e}")))?
                        .with_timezone(&Utc);
                    map.insert(slug, (hash, source, created));
                }
                Ok::<_, AthenError>(map)
            })
            .await
            .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))??;

        // 2. Walk `<skills_dir>/<slug>/SKILL.md`.
        let mut found: Vec<(String, SkillFrontmatter, String, PathBuf)> = Vec::new();
        let mut entries = tokio::fs::read_dir(&self.skills_dir)
            .await
            .map_err(|e| AthenError::Other(format!("Read skills dir: {e}")))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| AthenError::Other(format!("Iterate skills dir: {e}")))?
        {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let slug = match path.file_name().and_then(|n| n.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            if validate_slug(&slug).is_err() {
                tracing::warn!(slug = %slug, "skipping skill folder with invalid slug");
                continue;
            }
            let skill_md = path.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            let raw = match tokio::fs::read_to_string(&skill_md).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(slug = %slug, error = %e, "skipping skill: read failed");
                    continue;
                }
            };
            let (front, body) = match parse_skill_md(&raw) {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(slug = %slug, error = %e, "skipping skill: parse failed");
                    continue;
                }
            };
            // Hash the canonical re-serialization, not the raw bytes, so
            // incidental whitespace edits don't trip an "updated" diff.
            let canonical = serialize_skill_md(&front, &body);
            let hash = compute_hash(&canonical);
            found.push((slug, front, hash, skill_md));
        }

        // 3. Reconcile.
        let mut report = SyncReport::default();
        let mut seen: HashSet<String> = HashSet::new();
        for (slug, front, hash, body_path) in &found {
            seen.insert(slug.clone());
            match existing.get(slug) {
                None => {
                    let now = Utc::now();
                    self.insert_row(slug, front, hash, body_path, SkillSource::User, now, now)
                        .await?;
                    report.inserted += 1;
                }
                Some((existing_hash, source, created)) if existing_hash != hash => {
                    let now = Utc::now();
                    self.insert_row(slug, front, hash, body_path, *source, *created, now)
                        .await?;
                    report.updated += 1;
                }
                Some(_) => {}
            }
        }
        // 4. Delete index rows whose folders disappeared.
        let to_delete: Vec<String> = existing
            .keys()
            .filter(|s| !seen.contains(*s))
            .cloned()
            .collect();
        if !to_delete.is_empty() {
            let conn = self.conn.clone();
            let n = to_delete.len();
            tokio::task::spawn_blocking(move || {
                let c = conn.blocking_lock();
                let mut stmt = c
                    .prepare("DELETE FROM skills_index WHERE slug = ?1")
                    .map_err(|e| AthenError::Other(format!("Prepare sync delete: {e}")))?;
                for slug in to_delete {
                    stmt.execute(params![slug])
                        .map_err(|e| AthenError::Other(format!("Sync delete: {e}")))?;
                }
                Ok::<_, AthenError>(())
            })
            .await
            .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))??;
            report.deleted = n;
        }

        Ok(report)
    }
}

impl SqliteSkillStore {
    #[allow(clippy::too_many_arguments)]
    async fn insert_row(
        &self,
        slug: &str,
        front: &SkillFrontmatter,
        hash: &str,
        body_path: &Path,
        source: SkillSource,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    ) -> Result<()> {
        let conn = self.conn.clone();
        let slug_s = slug.to_string();
        let name = front.name.clone();
        let description = front.description.clone();
        let applies_to_json =
            serde_json::to_string(&front.applies_to).map_err(AthenError::Serialization)?;
        let source_s = source.as_str().to_string();
        let body_path_s = body_path.to_string_lossy().into_owned();
        let hash_s = hash.to_string();
        let created_s = created_at.to_rfc3339();
        let updated_s = updated_at.to_rfc3339();
        tokio::task::spawn_blocking(move || {
            let c = conn.blocking_lock();
            c.execute(
                "INSERT OR REPLACE INTO skills_index \
                 (slug, name, description, applies_to_json, source, body_path, hash, created_at, updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    slug_s,
                    name,
                    description,
                    applies_to_json,
                    source_s,
                    body_path_s,
                    hash_s,
                    created_s,
                    updated_s,
                ],
            )
            .map_err(|e| AthenError::Other(format!("Upsert skill: {e}")))?;
            Ok::<_, AthenError>(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::identity::ProfileTag;
    use tempfile::TempDir;

    async fn setup() -> (SqliteSkillStore, TempDir) {
        let dir = TempDir::new().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        let conn = Arc::new(Mutex::new(conn));
        init_schema(&conn).await.unwrap();
        let store = SqliteSkillStore::new(conn, dir.path().to_path_buf());
        (store, dir)
    }

    fn fm(name: &str, desc: &str, applies: Vec<ProfileTag>) -> SkillFrontmatter {
        SkillFrontmatter {
            name: name.into(),
            description: desc.into(),
            applies_to: applies,
        }
    }

    #[tokio::test]
    async fn upsert_writes_disk_and_index() {
        let (store, _dir) = setup().await;
        store
            .upsert(
                "cold-email",
                &fm(
                    "cold-email",
                    "Use for cold emails.",
                    vec![ProfileTag::Always],
                ),
                "# Body\n",
            )
            .await
            .unwrap();
        let s = store.get("cold-email").await.unwrap().unwrap();
        assert_eq!(s.slug, "cold-email");
        assert_eq!(s.source, SkillSource::User);
        assert!(s.body_path.exists());
        let body = store.load_body("cold-email").await.unwrap();
        assert_eq!(body, "# Body\n");
    }

    #[tokio::test]
    async fn upsert_preserves_created_at_and_source_on_replace() {
        let (store, _dir) = setup().await;
        store
            .upsert("x", &fm("x", "y", vec![ProfileTag::Always]), "v1")
            .await
            .unwrap();
        let first = store.get("x").await.unwrap().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        store
            .upsert("x", &fm("x", "y updated", vec![ProfileTag::Always]), "v2")
            .await
            .unwrap();
        let second = store.get("x").await.unwrap().unwrap();
        assert_eq!(second.created_at, first.created_at);
        assert!(second.updated_at > first.updated_at);
        assert_eq!(second.description, "y updated");
        assert_ne!(second.hash, first.hash);
    }

    #[tokio::test]
    async fn upsert_rejects_bad_slug() {
        let (store, _dir) = setup().await;
        for bad in ["", ".dotfile", "has space", "../escape", "with/slash"] {
            let err = store
                .upsert(bad, &fm("x", "y", vec![]), "body")
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("slug"),
                "wrong error for bad slug `{bad}`: {err}"
            );
        }
    }

    #[tokio::test]
    async fn upsert_rejects_empty_name_or_desc() {
        let (store, _dir) = setup().await;
        let err = store
            .upsert("s", &fm("   ", "desc", vec![]), "b")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("name"));
        let err = store
            .upsert("s", &fm("name", "   ", vec![]), "b")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("description"));
    }

    #[tokio::test]
    async fn list_filters_by_profile() {
        let (store, _dir) = setup().await;
        store
            .upsert("always-on", &fm("a", "a", vec![ProfileTag::Always]), "b")
            .await
            .unwrap();
        store
            .upsert(
                "coder-only",
                &fm("c", "c", vec![ProfileTag::Profile("coder".into())]),
                "b",
            )
            .await
            .unwrap();
        store
            .upsert(
                "not-coder",
                &fm("n", "n", vec![ProfileTag::NotProfile("coder".into())]),
                "b",
            )
            .await
            .unwrap();

        let all = store.list(None).await.unwrap();
        assert_eq!(all.len(), 3);

        let coder = store.list(Some("coder")).await.unwrap();
        let slugs: Vec<&str> = coder.iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(slugs, vec!["always-on", "coder-only"]);

        let outreach = store.list(Some("outreach")).await.unwrap();
        let slugs: Vec<&str> = outreach.iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(slugs, vec!["always-on", "not-coder"]);
    }

    #[tokio::test]
    async fn list_is_slug_sorted() {
        let (store, _dir) = setup().await;
        for slug in ["zeta", "alpha", "mu"] {
            store
                .upsert(slug, &fm(slug, slug, vec![ProfileTag::Always]), "b")
                .await
                .unwrap();
        }
        let all = store.list(None).await.unwrap();
        let slugs: Vec<&str> = all.iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(slugs, vec!["alpha", "mu", "zeta"]);
    }

    #[tokio::test]
    async fn delete_removes_disk_and_index() {
        let (store, _dir) = setup().await;
        store
            .upsert("gone", &fm("g", "g", vec![ProfileTag::Always]), "b")
            .await
            .unwrap();
        let folder = store.skills_dir().join("gone");
        assert!(folder.exists());
        store.delete("gone").await.unwrap();
        assert!(!folder.exists());
        assert!(store.get("gone").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_missing_errors() {
        let (store, _dir) = setup().await;
        let err = store.delete("nope").await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn sync_picks_up_manual_filesystem_drop() {
        let (store, dir) = setup().await;
        // User git-clones a skill into the directory by hand.
        let folder = dir.path().join("dropped-in");
        tokio::fs::create_dir_all(&folder).await.unwrap();
        let body = "---\nname: dropped-in\ndescription: Came from outside.\n---\n# Hello\n";
        tokio::fs::write(folder.join("SKILL.md"), body)
            .await
            .unwrap();

        let report = store.sync().await.unwrap();
        assert_eq!(report.inserted, 1);
        assert_eq!(report.updated, 0);
        assert_eq!(report.deleted, 0);

        let s = store.get("dropped-in").await.unwrap().unwrap();
        assert_eq!(s.source, SkillSource::User);
        assert_eq!(s.description, "Came from outside.");
    }

    #[tokio::test]
    async fn sync_detects_filesystem_edits() {
        let (store, dir) = setup().await;
        store
            .upsert("ed", &fm("ed", "v1", vec![ProfileTag::Always]), "body")
            .await
            .unwrap();
        let original_hash = store.get("ed").await.unwrap().unwrap().hash;

        // User edits SKILL.md by hand.
        let edited = "---\nname: ed\ndescription: v2\napplies_to: all\n---\nedited body\n";
        tokio::fs::write(dir.path().join("ed").join("SKILL.md"), edited)
            .await
            .unwrap();

        let report = store.sync().await.unwrap();
        assert_eq!(report.updated, 1);
        let updated = store.get("ed").await.unwrap().unwrap();
        assert_ne!(updated.hash, original_hash);
        assert_eq!(updated.description, "v2");
    }

    #[tokio::test]
    async fn sync_deletes_orphaned_index_rows() {
        let (store, dir) = setup().await;
        store
            .upsert("removed", &fm("r", "r", vec![ProfileTag::Always]), "body")
            .await
            .unwrap();
        // User rm -rf's the folder.
        tokio::fs::remove_dir_all(dir.path().join("removed"))
            .await
            .unwrap();

        let report = store.sync().await.unwrap();
        assert_eq!(report.deleted, 1);
        assert!(store.get("removed").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sync_is_idempotent_when_nothing_changed() {
        let (store, _dir) = setup().await;
        store
            .upsert("a", &fm("a", "a", vec![ProfileTag::Always]), "b")
            .await
            .unwrap();
        let r1 = store.sync().await.unwrap();
        // First sync sees the existing on-disk file as matching the index — no diff.
        assert_eq!(r1, SyncReport::default());
        let r2 = store.sync().await.unwrap();
        assert_eq!(r2, SyncReport::default());
    }

    #[tokio::test]
    async fn sync_skips_malformed_skill_md() {
        let (store, dir) = setup().await;
        let folder = dir.path().join("broken");
        tokio::fs::create_dir_all(&folder).await.unwrap();
        // No frontmatter opener → parser errors.
        tokio::fs::write(folder.join("SKILL.md"), "no frontmatter here")
            .await
            .unwrap();

        let report = store.sync().await.unwrap();
        assert_eq!(report.inserted, 0);
        assert!(store.get("broken").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sync_skips_invalid_slug_folder() {
        let (store, dir) = setup().await;
        let bad = dir.path().join(".hidden");
        tokio::fs::create_dir_all(&bad).await.unwrap();
        tokio::fs::write(bad.join("SKILL.md"), "---\nname: x\ndescription: y\n---\nz")
            .await
            .unwrap();
        let report = store.sync().await.unwrap();
        assert_eq!(report.inserted, 0);
    }

    #[tokio::test]
    async fn load_body_strips_frontmatter() {
        let (store, _dir) = setup().await;
        store
            .upsert(
                "fm",
                &fm("fm", "y", vec![ProfileTag::Always]),
                "actual body content",
            )
            .await
            .unwrap();
        let body = store.load_body("fm").await.unwrap();
        assert_eq!(body, "actual body content");
        assert!(!body.contains("---"));
        assert!(!body.contains("name:"));
    }

    #[tokio::test]
    async fn load_body_missing_errors() {
        let (store, _dir) = setup().await;
        let err = store.load_body("nope").await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
