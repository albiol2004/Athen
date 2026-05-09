//! Identity store types: hand-maintained personality, rules, knowledge, team
//! and any user-invented categories that should be true *across every agent*.
//!
//! Identity is Athen's "soul" — distinct from `athen-memory` (episodic facts
//! recalled per-query) and from `agent_profile` (defines *what* an agent does,
//! not *who* Athen is). Identity lives in the static prompt prefix so every
//! request that builds the same prefix re-uses the prompt cache.
//!
//! See `docs/IDENTITY.md` for the full design.
//!
//! Categories are first-class and user-editable. The four seeds Athen ships
//! (`personality`, `rules`, `knowledge`, `team`) are starting points, not a
//! closed enum — users add `coding_style`, `medical_history`, whatever they
//! need. Each entry declares an `applies_to` set (`Always`, specific profile
//! ids, or "everything except"), so the coder profile doesn't pay tokens for
//! personality entries that only matter to the personal-assistant.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::agent_profile::ProfileId;

/// One identity statement. Free-form markdown body, scoped to a category and
/// a set of profiles via `applies_to`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdentityEntry {
    pub id: Uuid,
    /// Category name — matches `IdentityCategory::name`. Free-form string.
    pub category: String,
    pub body: String,
    pub applies_to: Vec<ProfileTag>,
    /// User-flagged "always include even if budget is tight". v1 doesn't
    /// auto-truncate, but the flag is here so future budget code can prefer
    /// pinned entries when it has to drop something.
    pub pinned: bool,
    /// True when the agent (not the user) added this entry via the
    /// `identity_add` tool. The Settings UI shows a dismissible chip on
    /// these so the user can clear anything wrong with one click.
    #[serde(default)]
    pub proposed_by_agent: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Profile-applicability tag for identity entries. Resolves at prompt-build
/// time against the active profile id.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ProfileTag {
    /// Every agent reads this entry.
    Always,
    /// Only agents whose profile id matches.
    Profile(ProfileId),
    /// Every agent EXCEPT the one whose profile id matches. Power-user
    /// option for "applies broadly but not to coder".
    NotProfile(ProfileId),
}

impl ProfileTag {
    /// Does this tag include the given profile id?
    ///
    /// `Always` matches every profile. `Profile(p)` matches when `p == id`.
    /// `NotProfile(p)` matches when `p != id`.
    pub fn matches(&self, id: &str) -> bool {
        match self {
            ProfileTag::Always => true,
            ProfileTag::Profile(p) => p == id,
            ProfileTag::NotProfile(p) => p != id,
        }
    }
}

/// User-editable category that groups identity entries. Categories are
/// addressed by `name` (must be unique per user), and rendered in
/// `sort_order` ascending in the UI and in the prompt header.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdentityCategory {
    /// Primary key. Lowercase, snake_case by convention but not enforced.
    pub name: String,
    pub description: String,
    /// Suggested `applies_to` for new entries created in this category.
    /// The UI pre-fills the chip; users can override per-entry.
    pub default_applies_to: Vec<ProfileTag>,
    pub sort_order: u32,
    /// True for categories shipped by Athen (`personality`, `rules`,
    /// `knowledge`, `team`). Renaming or deleting a seed category is allowed
    /// but the UI surfaces a confirm. Re-creating it later just produces a
    /// user category with the same name; the seed flag does not return.
    pub is_seed: bool,
}

/// Returns true when an entry's `applies_to` matches a profile id. An entry
/// with an empty `applies_to` matches nothing — callers should treat empty
/// as "explicitly scoped to no profile" (a degenerate but legal state).
pub fn applies_to_profile(applies_to: &[ProfileTag], profile_id: &str) -> bool {
    applies_to.iter().any(|tag| tag.matches(profile_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_matches_any_profile() {
        assert!(ProfileTag::Always.matches("coder"));
        assert!(ProfileTag::Always.matches("personal_assistant"));
        assert!(ProfileTag::Always.matches(""));
    }

    #[test]
    fn profile_tag_exact_match() {
        let tag = ProfileTag::Profile("coder".into());
        assert!(tag.matches("coder"));
        assert!(!tag.matches("personal_assistant"));
    }

    #[test]
    fn not_profile_excludes_only_named() {
        let tag = ProfileTag::NotProfile("coder".into());
        assert!(!tag.matches("coder"));
        assert!(tag.matches("personal_assistant"));
        assert!(tag.matches("default"));
    }

    #[test]
    fn applies_to_profile_any_match_wins() {
        let tags = vec![
            ProfileTag::Profile("coder".into()),
            ProfileTag::Profile("outreach".into()),
        ];
        assert!(applies_to_profile(&tags, "coder"));
        assert!(applies_to_profile(&tags, "outreach"));
        assert!(!applies_to_profile(&tags, "personal_assistant"));
    }

    #[test]
    fn applies_to_profile_empty_matches_nothing() {
        let tags: Vec<ProfileTag> = vec![];
        assert!(!applies_to_profile(&tags, "coder"));
    }

    #[test]
    fn applies_to_profile_always_dominates() {
        let tags = vec![ProfileTag::NotProfile("coder".into()), ProfileTag::Always];
        // Always wins the OR even though NotProfile would exclude coder.
        assert!(applies_to_profile(&tags, "coder"));
    }
}
