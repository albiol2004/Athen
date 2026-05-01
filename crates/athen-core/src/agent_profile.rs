//! Agent profile types: composable personas with their own tool surface
//! and routing signals.
//!
//! A profile is the unit of customization for the agent. Built-in profiles
//! (default, outreach, personal_assistant, …) ship as seeded rows in
//! `agent_profiles`; user-authored profiles share the same shape and code
//! path so custom agents are first-class participants in routing and
//! delegation.
//!
//! Profiles assemble their system prompt from `PersonaTemplate` fragments
//! (categorized by `PersonaCategory`) plus an optional free-form addendum.
//! The persona slots in `athen-agent::executor::build_persona_header` and
//! `build_persona_rules` are what these templates ultimately replace —
//! workspace rules and the tool index stay non-overridable.
//!
//! IDs are human-readable strings (e.g. "default", "outreach") rather than
//! UUIDs so users can refer to them stably in URLs, hotkeys, and tool
//! arguments without copy-pasting opaque identifiers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub type ProfileId = String;
pub type TemplateId = String;

/// A composable system-prompt fragment. Fragments are categorized so the UI
/// can present a "Voice", "Mission", "Constraints", "OutputStyle" picker
/// instead of one giant textarea.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersonaTemplate {
    pub id: TemplateId,
    pub display_name: String,
    pub category: PersonaCategory,
    pub body: String,
    pub builtin: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum PersonaCategory {
    /// Tone, formality, language, persona voice.
    Voice,
    /// What this agent is here to do — goals, focus areas, success criteria.
    Mission,
    /// Hard limits, refusals, escalation triggers.
    Constraints,
    /// Reply formatting, length, citation style.
    OutputStyle,
}

/// Which tools a profile can call. `All` is the default; profiles
/// progressively restrict via `Groups` (group-id whitelist), `Explicit`
/// (exact-name whitelist), or `Deny` (start from all, subtract these).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToolSelection {
    #[default]
    All,
    Groups(Vec<String>),
    Explicit(Vec<String>),
    Deny(Vec<String>),
}

/// Closed enum of high-level domains a profile can claim expertise in.
/// Drives Coordinator routing — incoming tasks are classified into a
/// `DomainTag` + `TaskKindTag`, and profiles are scored by tag overlap.
///
/// New variants are additive only. The escape hatch for things this enum
/// doesn't cover is `ExpertiseDeclaration::strengths` (free-form, matched
/// via embeddings).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum DomainTag {
    Email,
    Calendar,
    Messaging,
    Coding,
    Research,
    Outreach,
    Marketing,
    Finance,
    Scheduling,
    DataAnalysis,
    Writing,
    Translation,
    Other,
}

/// Closed enum of task kinds (verb-shaped, vs. `DomainTag` which is
/// noun-shaped). A "draft a follow-up email" task is `Drafting` × `Email`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum TaskKindTag {
    Drafting,
    Editing,
    Summarizing,
    Researching,
    Scheduling,
    CodeReview,
    Coding,
    Debugging,
    DataAnalysis,
    Outreach,
    Triage,
    Other,
}

/// What a profile is (and isn't) good at. Used by the Coordinator to score
/// profile fit against a classified task.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExpertiseDeclaration {
    pub domains: Vec<DomainTag>,
    pub task_kinds: Vec<TaskKindTag>,
    /// ISO 639-1 codes or free-form names ("rust", "python", "es", "en").
    /// Matched fuzzily against task language hints.
    pub languages: Vec<String>,
    /// Free-form strengths matched via embedding similarity. Escape hatch
    /// for things the closed enums don't cover.
    pub strengths: Vec<String>,
    /// Explicit anti-match. Lowers score when present in the classified
    /// task — e.g. an outreach profile lists `Coding` here so coding tasks
    /// don't get routed to it even if no other profile claims them.
    pub avoid: Vec<TaskKindTag>,
}

/// A user- or system-defined agent persona with its own tools and routing
/// signals. Built-ins seed on first launch with `builtin: true` and cannot
/// be deleted (only cloned). User-authored profiles have `builtin: false`
/// and are fully owned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentProfile {
    pub id: ProfileId,
    pub display_name: String,
    pub description: String,
    /// Persona fragments composed in declared order to build the persona
    /// portion of the system prompt.
    pub persona_template_ids: Vec<TemplateId>,
    /// Free-form text appended after the composed templates. Empty for
    /// purely template-driven profiles.
    pub custom_persona_addendum: Option<String>,
    pub tool_selection: ToolSelection,
    pub expertise: ExpertiseDeclaration,
    /// Optional override of the LLM model profile name (matched against
    /// `athen_core::llm::ModelProfile`). When `None`, the default model
    /// profile is used.
    pub model_profile_hint: Option<String>,
    pub builtin: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl AgentProfile {
    /// ID of the seeded built-in default profile. Used as the fallback when
    /// an arc has no `active_profile_id` set, and as the destination for the
    /// "Reset to default" UX action.
    pub const DEFAULT_ID: &'static str = "default";
}

/// A profile bundled with its resolved persona templates, ready to drive
/// executor behavior. The composition root produces this by looking up a
/// profile id and resolving its `persona_template_ids` against
/// `ProfileStore::resolve_templates`.
///
/// Lives in `athen-core` so the executor (which doesn't depend on
/// persistence) can accept it as a parameter, and the delegation path
/// can pass the same shape into a sub-agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedAgentProfile {
    pub profile: AgentProfile,
    /// Persona templates in the order declared by `profile.persona_template_ids`.
    /// Missing template ids are silently dropped during resolution.
    pub persona_templates: Vec<PersonaTemplate>,
}

impl ResolvedAgentProfile {
    /// Whether this profile carries any custom persona content. When `false`,
    /// the executor falls back to its hardcoded persona text — i.e. today's
    /// "You are Athen" behavior. The seeded default profile satisfies this.
    pub fn has_custom_persona(&self) -> bool {
        !self.persona_templates.is_empty()
            || self.profile.custom_persona_addendum.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_selection_default_is_all() {
        assert_eq!(ToolSelection::default(), ToolSelection::All);
    }

    #[test]
    fn agent_profile_round_trips_through_json() {
        let now = Utc::now();
        let profile = AgentProfile {
            id: "outreach".to_string(),
            display_name: "Outreach Agent".to_string(),
            description: "Lead generation and follow-up specialist.".to_string(),
            persona_template_ids: vec!["concise_voice".into(), "outreach_mission".into()],
            custom_persona_addendum: Some("Always personalize first lines.".into()),
            tool_selection: ToolSelection::Groups(vec!["email".into(), "contacts".into()]),
            expertise: ExpertiseDeclaration {
                domains: vec![DomainTag::Outreach, DomainTag::Email],
                task_kinds: vec![TaskKindTag::Drafting, TaskKindTag::Outreach],
                languages: vec!["en".into(), "es".into()],
                strengths: vec!["cold-email subject lines".into()],
                avoid: vec![TaskKindTag::Coding, TaskKindTag::Debugging],
            },
            model_profile_hint: Some("Smart".into()),
            builtin: false,
            created_at: now,
            updated_at: now,
        };

        let json = serde_json::to_string(&profile).expect("serialize");
        let decoded: AgentProfile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(profile, decoded);
    }

    #[test]
    fn default_id_is_stable() {
        assert_eq!(AgentProfile::DEFAULT_ID, "default");
    }
}
