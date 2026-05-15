//! Skill persistence port. Implementation lives in `athen-persistence`.
//!
//! Skills are user-authored procedural playbooks the agent loads on demand.
//! See `crate::skill` for the types and the on-disk format, and
//! `docs/SKILLS.md` for the full design.
//!
//! Storage is hybrid: bodies live on disk as plain `SKILL.md` files (source
//! of truth, human-editable, git-friendly), and SQLite holds a derived index
//! for cheap listing. Implementations expose [`SkillStore::sync`] for the
//! boot-time reconciliation pass.

use async_trait::async_trait;

use crate::error::Result;
use crate::skill::{Skill, SkillFrontmatter};

/// Storage for skills.
///
/// Listing returns metadata only — bodies are loaded lazily via
/// [`SkillStore::load_body`] when the agent calls the `load_skill` tool.
///
/// Ordering: implementations return skills sorted by `slug ASC` (stable
/// across requests so the static-prefix cache stays valid).
///
/// Shadowing: when a `User` skill and a `Bundled` skill share a slug, the
/// `User` skill wins in listings. Implementations enforce this; callers
/// don't need to dedup.
#[async_trait]
pub trait SkillStore: Send + Sync {
    /// List skills whose `applies_to` matches the given profile id. Pass
    /// `None` to list every skill regardless of profile (used by the
    /// Settings UI). Bodies are NOT loaded.
    async fn list(&self, profile: Option<&str>) -> Result<Vec<Skill>>;

    /// Look up a single skill by slug. Returns `None` if no skill with that
    /// slug is indexed.
    async fn get(&self, slug: &str) -> Result<Option<Skill>>;

    /// Read the body half of `SKILL.md` for the given slug. The frontmatter
    /// is stripped — callers receive only the markdown body the agent will
    /// consume. Returns an error if the slug isn't indexed or the file is
    /// missing.
    async fn load_body(&self, slug: &str) -> Result<String>;

    /// Create or overwrite a skill. Writes `SKILL.md` to disk (canonical
    /// path = `<skills_dir>/<slug>/SKILL.md`) AND updates the index in one
    /// step. The implementation is responsible for computing the content
    /// hash and bumping `updated_at`. `source` is `User` for fresh upserts
    /// from the UI; importers and bundled-default writers go through their
    /// own entry points if needed.
    async fn upsert(&self, slug: &str, frontmatter: &SkillFrontmatter, body: &str) -> Result<()>;

    /// Delete a skill — removes both the on-disk folder (including any
    /// sibling files inside it) and the index row. Returns an error if the
    /// slug isn't indexed.
    async fn delete(&self, slug: &str) -> Result<()>;

    /// Reconcile the SQLite index against the filesystem. Walks the skills
    /// directory, parses each `SKILL.md`, inserts new rows, updates rows
    /// whose hash changed, and deletes rows whose files vanished. Idempotent
    /// — safe to call on every boot and after manual imports.
    ///
    /// Returns the number of `(inserted, updated, deleted)` rows so callers
    /// can log meaningful sync output.
    async fn sync(&self) -> Result<SyncReport>;
}

/// Counters returned by [`SkillStore::sync`] for boot-time observability.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub inserted: usize,
    pub updated: usize,
    pub deleted: usize,
}
