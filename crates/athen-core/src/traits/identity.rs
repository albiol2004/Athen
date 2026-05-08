//! Identity persistence port. Implementation lives in `athen-persistence`.

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::Result;
use crate::identity::{IdentityCategory, IdentityEntry};

/// Storage for identity categories and entries.
///
/// Implementations seed the four canonical categories (`personality`,
/// `rules`, `knowledge`, `team`) on first use. Seed categories can be renamed
/// or deleted by the user; the seed flag is a UI hint, not a protection.
///
/// Listing is always ordered by `sort_order ASC` for categories and
/// `(category, updated_at DESC, id ASC)` for entries — stable across requests
/// so the prompt-cache prefix stays valid.
#[async_trait]
pub trait IdentityStore: Send + Sync {
    // --- Categories ---

    /// List all categories ordered by `sort_order` ascending.
    async fn list_categories(&self) -> Result<Vec<IdentityCategory>>;

    /// Look up a category by name.
    async fn get_category(&self, name: &str) -> Result<Option<IdentityCategory>>;

    /// Insert or replace a category. The caller is responsible for picking a
    /// `sort_order` that fits — implementations don't auto-renumber.
    async fn upsert_category(&self, category: &IdentityCategory) -> Result<()>;

    /// Delete a category and cascade-delete its entries. Returns an error if
    /// the category doesn't exist.
    async fn delete_category(&self, name: &str) -> Result<()>;

    // --- Entries ---

    /// List all entries, optionally scoped to a single category. Entries are
    /// ordered `(category sort_order, updated_at DESC, id ASC)`.
    async fn list_entries(&self, category: Option<&str>) -> Result<Vec<IdentityEntry>>;

    /// Look up an entry by id.
    async fn get_entry(&self, id: Uuid) -> Result<Option<IdentityEntry>>;

    /// Insert or replace an entry. Sets `updated_at` to "now" regardless of
    /// what the caller passed; preserves `created_at` on replace.
    async fn upsert_entry(&self, entry: &IdentityEntry) -> Result<()>;

    /// Delete an entry by id. Returns an error if the entry doesn't exist.
    async fn delete_entry(&self, id: Uuid) -> Result<()>;

    /// Convenience: list all entries whose `applies_to` matches the given
    /// profile id, grouped by category in display order. Used by the prompt
    /// builder. Returned tuples are `(category, entries_for_category)`;
    /// categories with no matching entries are omitted.
    async fn entries_for_profile(
        &self,
        profile_id: &str,
    ) -> Result<Vec<(IdentityCategory, Vec<IdentityEntry>)>>;
}
