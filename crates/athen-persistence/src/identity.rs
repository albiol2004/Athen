//! SQLite-backed `IdentityStore`.
//!
//! Two tables: `identity_categories` (user-editable groupings) and
//! `identity_entries` (the statements themselves). `applies_to` is stored as
//! a JSON column on each entry — entries are read holistically, never
//! queried by tag, so a junction table would be over-engineered.
//!
//! On first init the four canonical seed categories (`personality`,
//! `rules`, `knowledge`, `team`) are inserted. Seeds can be renamed or
//! deleted by the user; the seed flag is informational.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::identity::{IdentityCategory, IdentityEntry, ProfileTag};
use athen_core::traits::identity::IdentityStore;

const IDENTITY_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS identity_categories (
    name TEXT PRIMARY KEY,
    description TEXT NOT NULL DEFAULT '',
    default_applies_to_json TEXT NOT NULL,
    sort_order INTEGER NOT NULL,
    is_seed INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS identity_entries (
    id TEXT PRIMARY KEY,
    category TEXT NOT NULL REFERENCES identity_categories(name) ON DELETE CASCADE,
    body TEXT NOT NULL,
    applies_to_json TEXT NOT NULL,
    pinned INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_identity_entries_category ON identity_entries(category);
"#;

/// SQLite-backed identity store.
pub struct SqliteIdentityStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteIdentityStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            // ON DELETE CASCADE on identity_entries.category requires foreign
            // keys to be enabled. SQLite defaults to off; flip per-connection.
            conn.execute_batch("PRAGMA foreign_keys = ON;")
                .map_err(|e| AthenError::Other(format!("Enable FK: {e}")))?;
            conn.execute_batch(IDENTITY_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Init identity schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    /// Seed the four canonical categories on first init. Idempotent: each
    /// seed is inserted only when its name is missing, so a user who has
    /// renamed `personality` won't have a fresh `personality` row reappear.
    pub async fn seed_categories_if_empty(&self) -> Result<()> {
        for cat in seed_categories() {
            if self.get_category(&cat.name).await?.is_none() {
                self.upsert_category_raw(&cat).await?;
            }
        }
        Ok(())
    }

    /// Internal upsert that bypasses the public path so the seeder can run
    /// before any user data exists.
    async fn upsert_category_raw(&self, category: &IdentityCategory) -> Result<()> {
        let conn = self.conn.clone();
        let cat = category.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let applies_to_json = serde_json::to_string(&cat.default_applies_to)
                .map_err(AthenError::Serialization)?;
            conn.execute(
                "INSERT OR REPLACE INTO identity_categories \
                 (name, description, default_applies_to_json, sort_order, is_seed) \
                 VALUES (?1,?2,?3,?4,?5)",
                params![
                    cat.name,
                    cat.description,
                    applies_to_json,
                    cat.sort_order as i64,
                    cat.is_seed as i64,
                ],
            )
            .map_err(|e| AthenError::Other(format!("Insert category: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

/// Canonical seed categories shipped on first launch.
///
/// Adding a new entry here ships it on next launch (existing installs pick
/// it up via `seed_categories_if_empty`'s per-name check). The order
/// reflects suggested prompt-order: personality first (sets voice), rules
/// (hard constraints), knowledge (about the user), team (organizational).
fn seed_categories() -> Vec<IdentityCategory> {
    vec![
        IdentityCategory {
            name: "personality".into(),
            description: "Voice, warmth, refusal style, humor level. How Athen sounds to people."
                .into(),
            default_applies_to: vec![ProfileTag::Always],
            sort_order: 10,
            is_seed: true,
        },
        IdentityCategory {
            name: "rules".into(),
            description: "Hard constraints — 'never X', 'always Y'. Survives profile switches."
                .into(),
            default_applies_to: vec![ProfileTag::Always],
            sort_order: 20,
            is_seed: true,
        },
        IdentityCategory {
            name: "knowledge".into(),
            description: "Facts about the user, family, recurring contexts.".into(),
            default_applies_to: vec![ProfileTag::Always],
            sort_order: 30,
            is_seed: true,
        },
        IdentityCategory {
            name: "team".into(),
            description: "Org chart, business identity, escalation chain. For business use.".into(),
            default_applies_to: vec![
                ProfileTag::Profile("assistant".into()),
                ProfileTag::Profile("outreach".into()),
            ],
            sort_order: 40,
            is_seed: true,
        },
    ]
}

const CATEGORY_COLS: &str = "name, description, default_applies_to_json, sort_order, is_seed";
const ENTRY_COLS: &str = "id, category, body, applies_to_json, pinned, created_at, updated_at";

fn read_category_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IdentityCategory> {
    let name: String = row.get(0)?;
    let description: String = row.get(1)?;
    let default_applies_to_json: String = row.get(2)?;
    let sort_order: i64 = row.get(3)?;
    let is_seed: i64 = row.get(4)?;
    let default_applies_to: Vec<ProfileTag> = serde_json::from_str(&default_applies_to_json)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e))
        })?;
    Ok(IdentityCategory {
        name,
        description,
        default_applies_to,
        sort_order: sort_order as u32,
        is_seed: is_seed != 0,
    })
}

fn read_entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(IdentityEntry, ())> {
    let id_str: String = row.get(0)?;
    let category: String = row.get(1)?;
    let body: String = row.get(2)?;
    let applies_to_json: String = row.get(3)?;
    let pinned: i64 = row.get(4)?;
    let created_at: String = row.get(5)?;
    let updated_at: String = row.get(6)?;

    let id = Uuid::parse_str(&id_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let applies_to: Vec<ProfileTag> = serde_json::from_str(&applies_to_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
    })?;
    let created_at = chrono::DateTime::parse_from_rfc3339(&created_at)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&chrono::Utc);
    let updated_at = chrono::DateTime::parse_from_rfc3339(&updated_at)
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?
        .with_timezone(&chrono::Utc);

    Ok((
        IdentityEntry {
            id,
            category,
            body,
            applies_to,
            pinned: pinned != 0,
            created_at,
            updated_at,
        },
        (),
    ))
}

#[async_trait]
impl IdentityStore for SqliteIdentityStore {
    async fn list_categories(&self) -> Result<Vec<IdentityCategory>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!(
                "SELECT {CATEGORY_COLS} FROM identity_categories \
                 ORDER BY sort_order ASC, name ASC"
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list_categories: {e}")))?;
            let rows = stmt
                .query_map([], read_category_row)
                .map_err(|e| AthenError::Other(format!("Query categories: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r.map_err(|e| AthenError::Other(format!("Category row: {e}")))?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn get_category(&self, name: &str) -> Result<Option<IdentityCategory>> {
        let conn = self.conn.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {CATEGORY_COLS} FROM identity_categories WHERE name = ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare get_category: {e}")))?;
            stmt.query_row(params![name], read_category_row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!("Query category: {other}"))),
                })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn upsert_category(&self, category: &IdentityCategory) -> Result<()> {
        // Public path: same body as raw, kept symmetric in case future
        // versions want to validate (e.g. reject empty names) at this layer.
        if category.name.trim().is_empty() {
            return Err(AthenError::Other(
                "Category name cannot be empty".to_string(),
            ));
        }
        self.upsert_category_raw(category).await
    }

    async fn delete_category(&self, name: &str) -> Result<()> {
        let conn = self.conn.clone();
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            // Foreign-key cascade requires per-connection PRAGMA. spawn_blocking
            // gives us a fresh borrow each call, so set it before the delete.
            conn.execute_batch("PRAGMA foreign_keys = ON;")
                .map_err(|e| AthenError::Other(format!("Enable FK: {e}")))?;
            let n = conn
                .execute(
                    "DELETE FROM identity_categories WHERE name = ?1",
                    params![name],
                )
                .map_err(|e| AthenError::Other(format!("Delete category: {e}")))?;
            if n == 0 {
                return Err(AthenError::Other(format!("Category not found: {name}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn list_entries(&self, category: Option<&str>) -> Result<Vec<IdentityEntry>> {
        let conn = self.conn.clone();
        let category = category.map(|s| s.to_string());
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut out = Vec::new();
            match category {
                Some(c) => {
                    let sql = format!(
                        "SELECT {ENTRY_COLS} FROM identity_entries \
                         WHERE category = ?1 \
                         ORDER BY updated_at DESC, id ASC"
                    );
                    let mut stmt = conn
                        .prepare(&sql)
                        .map_err(|e| AthenError::Other(format!("Prepare list_entries: {e}")))?;
                    let rows = stmt
                        .query_map(params![c], read_entry_row)
                        .map_err(|e| AthenError::Other(format!("Query entries: {e}")))?;
                    for r in rows {
                        let (entry, _) =
                            r.map_err(|e| AthenError::Other(format!("Entry row: {e}")))?;
                        out.push(entry);
                    }
                }
                None => {
                    // Cross-category listing joins on sort_order so the prompt
                    // builder can iterate in display order without a second
                    // query.
                    let sql = format!(
                        "SELECT {} FROM identity_entries e \
                         JOIN identity_categories c ON c.name = e.category \
                         ORDER BY c.sort_order ASC, c.name ASC, e.updated_at DESC, e.id ASC",
                        ENTRY_COLS
                            .split(',')
                            .map(|c| format!("e.{}", c.trim()))
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                    let mut stmt = conn
                        .prepare(&sql)
                        .map_err(|e| AthenError::Other(format!("Prepare list_entries: {e}")))?;
                    let rows = stmt
                        .query_map([], read_entry_row)
                        .map_err(|e| AthenError::Other(format!("Query entries: {e}")))?;
                    for r in rows {
                        let (entry, _) =
                            r.map_err(|e| AthenError::Other(format!("Entry row: {e}")))?;
                        out.push(entry);
                    }
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn get_entry(&self, id: Uuid) -> Result<Option<IdentityEntry>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {ENTRY_COLS} FROM identity_entries WHERE id = ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare get_entry: {e}")))?;
            stmt.query_row(params![id.to_string()], read_entry_row)
                .map(|(entry, _)| Some(entry))
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!("Query entry: {other}"))),
                })
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn upsert_entry(&self, entry: &IdentityEntry) -> Result<()> {
        let conn = self.conn.clone();
        let mut e = entry.clone();
        // Stamp updated_at; preserve created_at on replace by reading first.
        let existing = self.get_entry(e.id).await?;
        if let Some(prev) = existing {
            e.created_at = prev.created_at;
        }
        e.updated_at = Utc::now();
        // Validate category exists — entries dangling on a missing category
        // would round-trip in JSON but never reach the prompt.
        if self.get_category(&e.category).await?.is_none() {
            return Err(AthenError::Other(format!(
                "Category '{}' does not exist; create it first",
                e.category
            )));
        }
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let applies_to_json =
                serde_json::to_string(&e.applies_to).map_err(AthenError::Serialization)?;
            conn.execute(
                "INSERT OR REPLACE INTO identity_entries \
                 (id, category, body, applies_to_json, pinned, created_at, updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    e.id.to_string(),
                    e.category,
                    e.body,
                    applies_to_json,
                    e.pinned as i64,
                    e.created_at.to_rfc3339(),
                    e.updated_at.to_rfc3339(),
                ],
            )
            .map_err(|err| AthenError::Other(format!("Insert entry: {err}")))?;
            Ok(())
        })
        .await
        .map_err(|err| AthenError::Other(format!("Spawn blocking: {err}")))?
    }

    async fn delete_entry(&self, id: Uuid) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let n = conn
                .execute(
                    "DELETE FROM identity_entries WHERE id = ?1",
                    params![id.to_string()],
                )
                .map_err(|e| AthenError::Other(format!("Delete entry: {e}")))?;
            if n == 0 {
                return Err(AthenError::Other(format!("Entry not found: {id}")));
            }
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn entries_for_profile(
        &self,
        profile_id: &str,
    ) -> Result<Vec<(IdentityCategory, Vec<IdentityEntry>)>> {
        let categories = self.list_categories().await?;
        let entries = self.list_entries(None).await?;
        let mut out: Vec<(IdentityCategory, Vec<IdentityEntry>)> = Vec::new();
        for cat in categories {
            let matched: Vec<IdentityEntry> = entries
                .iter()
                .filter(|e| e.category == cat.name)
                .filter(|e| athen_core::identity::applies_to_profile(&e.applies_to, profile_id))
                .cloned()
                .collect();
            if !matched.is_empty() {
                out.push((cat, matched));
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_store() -> SqliteIdentityStore {
        let conn = Connection::open_in_memory().unwrap();
        let store = SqliteIdentityStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.unwrap();
        store.seed_categories_if_empty().await.unwrap();
        store
    }

    fn mk_entry(category: &str, body: &str, applies_to: Vec<ProfileTag>) -> IdentityEntry {
        let now = Utc::now();
        IdentityEntry {
            id: Uuid::new_v4(),
            category: category.into(),
            body: body.into(),
            applies_to,
            pinned: false,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn seeds_four_canonical_categories() {
        let store = setup_store().await;
        let cats = store.list_categories().await.unwrap();
        let names: Vec<&str> = cats.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["personality", "rules", "knowledge", "team"]);
        assert!(cats.iter().all(|c| c.is_seed));
    }

    #[tokio::test]
    async fn seed_is_idempotent_and_preserves_user_edits() {
        let store = setup_store().await;
        // User renames `personality` → keeps the row but with a different
        // sort_order/desc.
        store
            .upsert_category(&IdentityCategory {
                name: "personality".into(),
                description: "Voice (custom)".into(),
                default_applies_to: vec![ProfileTag::Profile("assistant".into())],
                sort_order: 5,
                is_seed: true,
            })
            .await
            .unwrap();
        store.seed_categories_if_empty().await.unwrap();
        let cat = store.get_category("personality").await.unwrap().unwrap();
        assert_eq!(cat.description, "Voice (custom)");
        assert_eq!(cat.sort_order, 5);
    }

    #[tokio::test]
    async fn upsert_and_list_entries() {
        let store = setup_store().await;
        let e1 = mk_entry("personality", "Be warm.", vec![ProfileTag::Always]);
        let e2 = mk_entry(
            "rules",
            "Never auto-send to legal@.",
            vec![ProfileTag::Always],
        );
        store.upsert_entry(&e1).await.unwrap();
        store.upsert_entry(&e2).await.unwrap();
        let all = store.list_entries(None).await.unwrap();
        assert_eq!(all.len(), 2);
        // personality (sort_order 10) comes before rules (20).
        assert_eq!(all[0].category, "personality");
        assert_eq!(all[1].category, "rules");
    }

    #[tokio::test]
    async fn upsert_rejects_unknown_category() {
        let store = setup_store().await;
        let e = mk_entry("not_a_category", "...", vec![ProfileTag::Always]);
        let err = store.upsert_entry(&e).await.unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[tokio::test]
    async fn upsert_rejects_empty_category_name() {
        let store = setup_store().await;
        let err = store
            .upsert_category(&IdentityCategory {
                name: "   ".into(),
                description: "".into(),
                default_applies_to: vec![ProfileTag::Always],
                sort_order: 100,
                is_seed: false,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[tokio::test]
    async fn delete_category_cascades_to_entries() {
        let store = setup_store().await;
        // Create a user category with an entry.
        store
            .upsert_category(&IdentityCategory {
                name: "coding_style".into(),
                description: "Coding prefs".into(),
                default_applies_to: vec![ProfileTag::Profile("coder".into())],
                sort_order: 100,
                is_seed: false,
            })
            .await
            .unwrap();
        let e = mk_entry(
            "coding_style",
            "Prefer tracing over println.",
            vec![ProfileTag::Profile("coder".into())],
        );
        let id = e.id;
        store.upsert_entry(&e).await.unwrap();
        assert!(store.get_entry(id).await.unwrap().is_some());

        store.delete_category("coding_style").await.unwrap();
        assert!(store.get_category("coding_style").await.unwrap().is_none());
        assert!(store.get_entry(id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn delete_missing_category_errors() {
        let store = setup_store().await;
        let err = store.delete_category("nope").await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn delete_entry_rejects_missing_id() {
        let store = setup_store().await;
        let err = store.delete_entry(Uuid::new_v4()).await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn entries_for_profile_filters_by_applies_to() {
        let store = setup_store().await;
        // Add a `coding_style` user category.
        store
            .upsert_category(&IdentityCategory {
                name: "coding_style".into(),
                description: "Coding prefs".into(),
                default_applies_to: vec![ProfileTag::Profile("coder".into())],
                sort_order: 100,
                is_seed: false,
            })
            .await
            .unwrap();

        let always = mk_entry("personality", "Be warm.", vec![ProfileTag::Always]);
        let coder_only = mk_entry(
            "coding_style",
            "Prefer tracing.",
            vec![ProfileTag::Profile("coder".into())],
        );
        let assistant_only = mk_entry(
            "rules",
            "Confirm before deleting calendar events.",
            vec![ProfileTag::Profile("assistant".into())],
        );
        store.upsert_entry(&always).await.unwrap();
        store.upsert_entry(&coder_only).await.unwrap();
        store.upsert_entry(&assistant_only).await.unwrap();

        let coder_view = store.entries_for_profile("coder").await.unwrap();
        // coder sees: personality (always) + coding_style (profile-match).
        // Does NOT see rules entry (assistant-only).
        let cats: Vec<&str> = coder_view.iter().map(|(c, _)| c.name.as_str()).collect();
        assert_eq!(cats, vec!["personality", "coding_style"]);

        let assistant_view = store.entries_for_profile("assistant").await.unwrap();
        let cats: Vec<&str> = assistant_view
            .iter()
            .map(|(c, _)| c.name.as_str())
            .collect();
        assert_eq!(cats, vec!["personality", "rules"]);
    }

    #[tokio::test]
    async fn updated_at_advances_on_replace_created_at_preserved() {
        let store = setup_store().await;
        let mut e = mk_entry("personality", "v1", vec![ProfileTag::Always]);
        let original_created = e.created_at;
        store.upsert_entry(&e).await.unwrap();
        // Wait a tick so timestamps differ.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        e.body = "v2".into();
        store.upsert_entry(&e).await.unwrap();
        let loaded = store.get_entry(e.id).await.unwrap().unwrap();
        assert_eq!(loaded.body, "v2");
        assert_eq!(loaded.created_at, original_created);
        assert!(loaded.updated_at > original_created);
    }

    #[tokio::test]
    async fn list_entries_scoped_to_category() {
        let store = setup_store().await;
        store
            .upsert_entry(&mk_entry("personality", "a", vec![ProfileTag::Always]))
            .await
            .unwrap();
        store
            .upsert_entry(&mk_entry("rules", "b", vec![ProfileTag::Always]))
            .await
            .unwrap();
        let only = store.list_entries(Some("rules")).await.unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].body, "b");
    }
}
