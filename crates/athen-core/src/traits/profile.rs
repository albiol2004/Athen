//! Profile persistence port. Implementations live in `athen-persistence`.

use async_trait::async_trait;

use crate::agent_profile::{AgentProfile, PersonaTemplate, ProfileId, TemplateId};
use crate::error::Result;

/// Storage for `AgentProfile` and `PersonaTemplate` rows.
///
/// Implementations seed built-in rows on first use so a default profile is
/// always queryable. `delete_profile` and `delete_template` refuse to remove
/// built-ins — built-ins are clonable but not deletable, so a "Reset to
/// default" UX action always has somewhere to land.
#[async_trait]
pub trait ProfileStore: Send + Sync {
    /// Look up a profile by id. Returns `None` if no row matches.
    async fn get_profile(&self, id: &str) -> Result<Option<AgentProfile>>;

    /// List all profiles, ordered with built-ins first then by creation time.
    async fn list_profiles(&self) -> Result<Vec<AgentProfile>>;

    /// Insert or replace a profile. Updates `updated_at` to "now" on the
    /// stored row regardless of what the caller passed.
    async fn save_profile(&self, profile: &AgentProfile) -> Result<()>;

    /// Delete a user-authored profile. Returns an error if the profile is
    /// built-in or doesn't exist.
    async fn delete_profile(&self, id: &str) -> Result<()>;

    /// Look up a persona template by id.
    async fn get_template(&self, id: &str) -> Result<Option<PersonaTemplate>>;

    /// List all persona templates, built-ins first.
    async fn list_templates(&self) -> Result<Vec<PersonaTemplate>>;

    /// Insert or replace a persona template.
    async fn save_template(&self, template: &PersonaTemplate) -> Result<()>;

    /// Delete a user-authored template. Returns an error if the template is
    /// built-in or doesn't exist.
    async fn delete_template(&self, id: &str) -> Result<()>;

    /// Resolve a list of template ids into bodies, in the order requested.
    /// Missing template ids are silently skipped — callers should use
    /// `get_template` first if they need to validate existence.
    async fn resolve_templates(&self, ids: &[TemplateId]) -> Result<Vec<PersonaTemplate>>;

    /// Resolve a profile id to its full `AgentProfile`, falling back to
    /// `AgentProfile::DEFAULT_ID` when `id` is missing or unknown. Always
    /// returns Some — the default profile is seeded on first launch and
    /// cannot be deleted, so this is the authoritative "give me a profile to
    /// run with" entry point.
    async fn get_or_default(&self, id: Option<&ProfileId>) -> Result<AgentProfile>;
}
