//! SQLite-backed `ProfileStore`.
//!
//! Stores `AgentProfile` and `PersonaTemplate` rows. Most non-id fields that
//! are structured (`tool_selection`, `expertise`, `persona_template_ids`) are
//! stored as JSON text — these aren't queried relationally, just round-tripped.
//!
//! Built-in rows seed once on first init via `seed_builtins_if_empty`. The
//! seeded `default` profile has empty persona templates and `ToolSelection::All`,
//! so wiring it through the executor reproduces today's behavior byte-for-byte
//! (the executor's hardcoded "You are Athen…" header runs when `persona_template_ids`
//! is empty and `custom_persona_addendum` is `None`).
//!
//! Built-ins refuse deletion. They can be cloned via the UI to seed a
//! user-authored row, but the canonical `default` always exists for fallback.
//!
//! Why JSON over relational tables for tags/expertise: profiles are read
//! holistically (load the whole row to run an agent), never queried by tag
//! at scale. Coordinator routing iterates the full set in memory anyway —
//! we won't have thousands of profiles.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{params, Connection};
use tokio::sync::Mutex;

use athen_core::agent_profile::{
    AgentProfile, ExpertiseDeclaration, PersonaCategory, PersonaTemplate, ProfileId,
    TemplateId, ToolSelection,
};
use athen_core::error::{AthenError, Result};
use athen_core::traits::profile::ProfileStore;

const PROFILES_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS agent_profiles (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    description TEXT NOT NULL,
    persona_template_ids_json TEXT NOT NULL,
    custom_persona_addendum TEXT,
    tool_selection_json TEXT NOT NULL,
    expertise_json TEXT NOT NULL,
    model_profile_hint TEXT,
    builtin INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS persona_templates (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    category TEXT NOT NULL,
    body TEXT NOT NULL,
    builtin INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);
"#;

/// SQLite-backed profile store.
pub struct SqliteProfileStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteProfileStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn init_schema(&self) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute_batch(PROFILES_SCHEMA_SQL)
                .map_err(|e| AthenError::Other(format!("Init profiles schema: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    /// Insert the canonical built-in `default` profile if no built-in
    /// profile exists yet. Idempotent: safe to call on every boot.
    pub async fn seed_builtins_if_empty(&self) -> Result<()> {
        if self.get_profile(AgentProfile::DEFAULT_ID).await?.is_some() {
            return Ok(());
        }
        let now = Utc::now();
        let default = AgentProfile {
            id: AgentProfile::DEFAULT_ID.to_string(),
            display_name: "Athen (default)".to_string(),
            description:
                "Universal proactive AI agent. Reproduces Athen's original behavior — \
                 no profile-specific persona, no tool restrictions."
                    .to_string(),
            persona_template_ids: vec![],
            custom_persona_addendum: None,
            tool_selection: ToolSelection::All,
            expertise: ExpertiseDeclaration::default(),
            model_profile_hint: None,
            builtin: true,
            created_at: now,
            updated_at: now,
        };
        self.save_profile_raw(&default).await
    }

    /// Internal save that bypasses the built-in protection in `save_profile`.
    /// Used by `seed_builtins_if_empty` to insert built-in rows.
    async fn save_profile_raw(&self, profile: &AgentProfile) -> Result<()> {
        let conn = self.conn.clone();
        let p = profile.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let template_ids_json =
                serde_json::to_string(&p.persona_template_ids).map_err(AthenError::Serialization)?;
            let tool_selection_json =
                serde_json::to_string(&p.tool_selection).map_err(AthenError::Serialization)?;
            let expertise_json =
                serde_json::to_string(&p.expertise).map_err(AthenError::Serialization)?;
            conn.execute(
                "INSERT OR REPLACE INTO agent_profiles \
                 (id, display_name, description, persona_template_ids_json, \
                  custom_persona_addendum, tool_selection_json, expertise_json, \
                  model_profile_hint, builtin, created_at, updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    p.id,
                    p.display_name,
                    p.description,
                    template_ids_json,
                    p.custom_persona_addendum,
                    tool_selection_json,
                    expertise_json,
                    p.model_profile_hint,
                    p.builtin as i64,
                    p.created_at.to_rfc3339(),
                    p.updated_at.to_rfc3339(),
                ],
            )
            .map_err(|e| AthenError::Other(format!("Insert profile: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }
}

fn category_to_str(c: PersonaCategory) -> &'static str {
    match c {
        PersonaCategory::Voice => "Voice",
        PersonaCategory::Mission => "Mission",
        PersonaCategory::Constraints => "Constraints",
        PersonaCategory::OutputStyle => "OutputStyle",
    }
}

fn str_to_category(s: &str) -> Result<PersonaCategory> {
    match s {
        "Voice" => Ok(PersonaCategory::Voice),
        "Mission" => Ok(PersonaCategory::Mission),
        "Constraints" => Ok(PersonaCategory::Constraints),
        "OutputStyle" => Ok(PersonaCategory::OutputStyle),
        other => Err(AthenError::Other(format!("Unknown PersonaCategory: {other}"))),
    }
}

fn parse_datetime(s: &str) -> Result<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .map_err(|e| AthenError::Other(format!("Invalid datetime '{s}': {e}")))
}

struct ProfileRow {
    id: String,
    display_name: String,
    description: String,
    persona_template_ids_json: String,
    custom_persona_addendum: Option<String>,
    tool_selection_json: String,
    expertise_json: String,
    model_profile_hint: Option<String>,
    builtin: i64,
    created_at: String,
    updated_at: String,
}

fn row_to_profile(row: ProfileRow) -> Result<AgentProfile> {
    let persona_template_ids: Vec<TemplateId> =
        serde_json::from_str(&row.persona_template_ids_json).map_err(AthenError::Serialization)?;
    let tool_selection: ToolSelection =
        serde_json::from_str(&row.tool_selection_json).map_err(AthenError::Serialization)?;
    let expertise: ExpertiseDeclaration =
        serde_json::from_str(&row.expertise_json).map_err(AthenError::Serialization)?;
    Ok(AgentProfile {
        id: row.id,
        display_name: row.display_name,
        description: row.description,
        persona_template_ids,
        custom_persona_addendum: row.custom_persona_addendum,
        tool_selection,
        expertise,
        model_profile_hint: row.model_profile_hint,
        builtin: row.builtin != 0,
        created_at: parse_datetime(&row.created_at)?,
        updated_at: parse_datetime(&row.updated_at)?,
    })
}

const PROFILE_COLS: &str = "id, display_name, description, persona_template_ids_json, \
     custom_persona_addendum, tool_selection_json, expertise_json, \
     model_profile_hint, builtin, created_at, updated_at";

fn read_profile_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProfileRow> {
    Ok(ProfileRow {
        id: row.get(0)?,
        display_name: row.get(1)?,
        description: row.get(2)?,
        persona_template_ids_json: row.get(3)?,
        custom_persona_addendum: row.get(4)?,
        tool_selection_json: row.get(5)?,
        expertise_json: row.get(6)?,
        model_profile_hint: row.get(7)?,
        builtin: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

#[async_trait]
impl ProfileStore for SqliteProfileStore {
    async fn get_profile(&self, id: &str) -> Result<Option<AgentProfile>> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!("SELECT {PROFILE_COLS} FROM agent_profiles WHERE id = ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare get_profile: {e}")))?;
            let row_opt = stmt
                .query_row(params![id], read_profile_row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!("Query profile: {other}"))),
                })?;
            row_opt.map(row_to_profile).transpose()
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn list_profiles(&self) -> Result<Vec<AgentProfile>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let sql = format!(
                "SELECT {PROFILE_COLS} FROM agent_profiles \
                 ORDER BY builtin DESC, created_at ASC"
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AthenError::Other(format!("Prepare list_profiles: {e}")))?;
            let rows = stmt
                .query_map([], read_profile_row)
                .map_err(|e| AthenError::Other(format!("Query profiles: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                let row = r.map_err(|e| AthenError::Other(format!("Profile row: {e}")))?;
                out.push(row_to_profile(row)?);
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn save_profile(&self, profile: &AgentProfile) -> Result<()> {
        // Refuse to overwrite a built-in row's `builtin` flag from outside.
        // Existing built-ins are immutable in shape; user-driven edits should
        // clone first then save with builtin=false.
        if let Some(existing) = self.get_profile(&profile.id).await? {
            if existing.builtin && !profile.builtin {
                return Err(AthenError::Other(format!(
                    "Cannot overwrite built-in profile '{}' with a non-built-in row; \
                     clone it under a new id instead",
                    profile.id
                )));
            }
            if existing.builtin && profile.builtin {
                return Err(AthenError::Other(format!(
                    "Cannot mutate built-in profile '{}' in place; clone it first",
                    profile.id
                )));
            }
        }
        let mut p = profile.clone();
        p.updated_at = Utc::now();
        self.save_profile_raw(&p).await
    }

    async fn delete_profile(&self, id: &str) -> Result<()> {
        let existing = self.get_profile(id).await?;
        let Some(profile) = existing else {
            return Err(AthenError::Other(format!("Profile not found: {id}")));
        };
        if profile.builtin {
            return Err(AthenError::Other(format!(
                "Cannot delete built-in profile '{id}'; clone instead"
            )));
        }
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute("DELETE FROM agent_profiles WHERE id = ?1", params![id])
                .map_err(|e| AthenError::Other(format!("Delete profile: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn get_template(&self, id: &str) -> Result<Option<PersonaTemplate>> {
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, display_name, category, body, builtin, created_at \
                     FROM persona_templates WHERE id = ?1",
                )
                .map_err(|e| AthenError::Other(format!("Prepare get_template: {e}")))?;
            let row_opt = stmt
                .query_row(params![id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(AthenError::Other(format!("Query template: {other}"))),
                })?;
            match row_opt {
                None => Ok(None),
                Some((id, display_name, category, body, builtin, created_at)) => {
                    Ok(Some(PersonaTemplate {
                        id,
                        display_name,
                        category: str_to_category(&category)?,
                        body,
                        builtin: builtin != 0,
                        created_at: parse_datetime(&created_at)?,
                    }))
                }
            }
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn list_templates(&self) -> Result<Vec<PersonaTemplate>> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn
                .prepare(
                    "SELECT id, display_name, category, body, builtin, created_at \
                     FROM persona_templates ORDER BY builtin DESC, created_at ASC",
                )
                .map_err(|e| AthenError::Other(format!("Prepare list_templates: {e}")))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .map_err(|e| AthenError::Other(format!("Query templates: {e}")))?;
            let mut out = Vec::new();
            for r in rows {
                let (id, display_name, category, body, builtin, created_at) =
                    r.map_err(|e| AthenError::Other(format!("Template row: {e}")))?;
                out.push(PersonaTemplate {
                    id,
                    display_name,
                    category: str_to_category(&category)?,
                    body,
                    builtin: builtin != 0,
                    created_at: parse_datetime(&created_at)?,
                });
            }
            Ok(out)
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn save_template(&self, template: &PersonaTemplate) -> Result<()> {
        if let Some(existing) = self.get_template(&template.id).await? {
            if existing.builtin && !template.builtin {
                return Err(AthenError::Other(format!(
                    "Cannot overwrite built-in template '{}' with a non-built-in row",
                    template.id
                )));
            }
            if existing.builtin && template.builtin {
                return Err(AthenError::Other(format!(
                    "Cannot mutate built-in template '{}' in place",
                    template.id
                )));
            }
        }
        let conn = self.conn.clone();
        let t = template.clone();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT OR REPLACE INTO persona_templates \
                 (id, display_name, category, body, builtin, created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6)",
                params![
                    t.id,
                    t.display_name,
                    category_to_str(t.category),
                    t.body,
                    t.builtin as i64,
                    t.created_at.to_rfc3339(),
                ],
            )
            .map_err(|e| AthenError::Other(format!("Insert template: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn delete_template(&self, id: &str) -> Result<()> {
        let existing = self.get_template(id).await?;
        let Some(template) = existing else {
            return Err(AthenError::Other(format!("Template not found: {id}")));
        };
        if template.builtin {
            return Err(AthenError::Other(format!(
                "Cannot delete built-in template '{id}'"
            )));
        }
        let conn = self.conn.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute("DELETE FROM persona_templates WHERE id = ?1", params![id])
                .map_err(|e| AthenError::Other(format!("Delete template: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
    }

    async fn resolve_templates(&self, ids: &[TemplateId]) -> Result<Vec<PersonaTemplate>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(t) = self.get_template(id).await? {
                out.push(t);
            }
        }
        Ok(out)
    }

    async fn get_or_default(&self, id: Option<&ProfileId>) -> Result<AgentProfile> {
        if let Some(id) = id {
            if let Some(p) = self.get_profile(id).await? {
                return Ok(p);
            }
        }
        match self.get_profile(AgentProfile::DEFAULT_ID).await? {
            Some(p) => Ok(p),
            None => Err(AthenError::Other(
                "Default profile not seeded — call seed_builtins_if_empty first"
                    .to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_store() -> SqliteProfileStore {
        let conn = Connection::open_in_memory().unwrap();
        let store = SqliteProfileStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.unwrap();
        store.seed_builtins_if_empty().await.unwrap();
        store
    }

    #[tokio::test]
    async fn seeds_default_profile() {
        let store = setup_store().await;
        let default = store.get_profile(AgentProfile::DEFAULT_ID).await.unwrap();
        assert!(default.is_some());
        let p = default.unwrap();
        assert!(p.builtin);
        assert_eq!(p.tool_selection, ToolSelection::All);
        assert!(p.persona_template_ids.is_empty());
    }

    #[tokio::test]
    async fn seed_is_idempotent() {
        let store = setup_store().await;
        store.seed_builtins_if_empty().await.unwrap();
        store.seed_builtins_if_empty().await.unwrap();
        let all = store.list_profiles().await.unwrap();
        assert_eq!(all.len(), 1);
    }

    #[tokio::test]
    async fn save_and_load_user_profile() {
        let store = setup_store().await;
        let now = Utc::now();
        let p = AgentProfile {
            id: "marketing".into(),
            display_name: "Marketing Expert".into(),
            description: "Landing-page and outreach optimizer.".into(),
            persona_template_ids: vec!["concise_voice".into()],
            custom_persona_addendum: Some("Optimize for conversions.".into()),
            tool_selection: ToolSelection::Groups(vec!["web".into(), "email".into()]),
            expertise: ExpertiseDeclaration {
                domains: vec![athen_core::agent_profile::DomainTag::Marketing],
                ..Default::default()
            },
            model_profile_hint: None,
            builtin: false,
            created_at: now,
            updated_at: now,
        };
        store.save_profile(&p).await.unwrap();
        let loaded = store.get_profile("marketing").await.unwrap().unwrap();
        assert_eq!(loaded.display_name, "Marketing Expert");
        assert_eq!(
            loaded.tool_selection,
            ToolSelection::Groups(vec!["web".into(), "email".into()])
        );
    }

    #[tokio::test]
    async fn cannot_delete_builtin() {
        let store = setup_store().await;
        let err = store.delete_profile(AgentProfile::DEFAULT_ID).await.unwrap_err();
        assert!(err.to_string().contains("Cannot delete built-in"));
    }

    #[tokio::test]
    async fn cannot_overwrite_builtin_in_place() {
        let store = setup_store().await;
        let mut default = store.get_profile(AgentProfile::DEFAULT_ID).await.unwrap().unwrap();
        default.display_name = "hacked".into();
        let err = store.save_profile(&default).await.unwrap_err();
        assert!(err.to_string().contains("built-in"));
    }

    #[tokio::test]
    async fn delete_user_profile_works() {
        let store = setup_store().await;
        let now = Utc::now();
        let p = AgentProfile {
            id: "tmp".into(),
            display_name: "tmp".into(),
            description: String::new(),
            persona_template_ids: vec![],
            custom_persona_addendum: None,
            tool_selection: ToolSelection::All,
            expertise: ExpertiseDeclaration::default(),
            model_profile_hint: None,
            builtin: false,
            created_at: now,
            updated_at: now,
        };
        store.save_profile(&p).await.unwrap();
        store.delete_profile("tmp").await.unwrap();
        assert!(store.get_profile("tmp").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_or_default_falls_back() {
        let store = setup_store().await;
        let p = store.get_or_default(Some(&"nonexistent".to_string())).await.unwrap();
        assert_eq!(p.id, AgentProfile::DEFAULT_ID);
        let p2 = store.get_or_default(None).await.unwrap();
        assert_eq!(p2.id, AgentProfile::DEFAULT_ID);
    }

    #[tokio::test]
    async fn save_and_resolve_templates() {
        let store = setup_store().await;
        let now = Utc::now();
        let t = PersonaTemplate {
            id: "concise_voice".into(),
            display_name: "Concise Voice".into(),
            category: PersonaCategory::Voice,
            body: "Reply tersely.".into(),
            builtin: false,
            created_at: now,
        };
        store.save_template(&t).await.unwrap();
        let resolved = store
            .resolve_templates(&["concise_voice".into(), "missing".into()])
            .await
            .unwrap();
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].body, "Reply tersely.");
    }
}
