//! Heuristic task classifier + profile scorer that route a task to the
//! most appropriate `AgentProfile` at arc-creation time.
//!
//! Two pure pieces: `classify_task` turns (source, description) into a
//! `ClassifiedTask`, and `pick_profile` scores every candidate profile
//! against the classification and returns the winner (or `None` if no
//! profile beats the default).
//!
//! Both pieces live in `athen-core` as free functions so the
//! composition root can call them without depending on the persistence
//! crate, and tests can drive them with synthetic profiles.
//!
//! Today: keyword-based classification, simple weighted scoring. The
//! escape hatches are explicit:
//! - `ExpertiseDeclaration::strengths` is reserved for embedding-similarity
//!   matching (not implemented in this MVP).
//! - LLM-based classification is the next-tier upgrade if heuristics
//!   misroute too often.
//!
//! Trust note: the router's job is to *propose* a profile. The composition
//! root is responsible for surfacing the decision to the user (so they can
//! manually override) and for falling back to the default profile when
//! `pick_profile` returns `None`.

use crate::agent_profile::{AgentProfile, DomainTag, ProfileId, TaskKindTag};

/// What we inferred about a task from its source channel and description.
/// Any field can be `None` — better to be uncertain than to misroute.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClassifiedTask {
    pub domain: Option<DomainTag>,
    pub kind: Option<TaskKindTag>,
    /// ISO 639-1 code or free-form name. None for now (no language detection
    /// in this MVP); reserved for a future langdetect-based enrichment.
    pub language: Option<String>,
}

/// Inspect (source, description) and return whatever we can confidently say
/// about the task. The source channel is the strongest signal — emails are
/// always email-domain tasks regardless of what the body says — so it
/// drives the domain. The description drives the task kind via keyword
/// match. Conservative on purpose: returns `None` for fields we can't
/// reliably tag rather than guessing.
pub fn classify_task(source: Option<&str>, description: &str) -> ClassifiedTask {
    let domain = match source {
        Some("email") => Some(DomainTag::Email),
        Some("calendar") => Some(DomainTag::Calendar),
        Some("messaging") => Some(DomainTag::Messaging),
        _ => None,
    };

    let lower = description.to_lowercase();
    let kind = if contains_any(&lower, &["draft", "write", "compose", "reply", "respond"]) {
        Some(TaskKindTag::Drafting)
    } else if contains_any(
        &lower,
        &["schedule", "meeting", "appointment", "book", "calendar"],
    ) {
        Some(TaskKindTag::Scheduling)
    } else if contains_any(&lower, &["debug", "fix the bug", "stack trace", "error in"]) {
        Some(TaskKindTag::Debugging)
    } else if contains_any(&lower, &["code", "implement", "function", "refactor", "module"])
    {
        Some(TaskKindTag::Coding)
    } else if contains_any(&lower, &["review", "critique", "feedback on"]) {
        Some(TaskKindTag::CodeReview)
    } else if contains_any(
        &lower,
        &["research", "investigate", "look up", "find out", "what is"],
    ) {
        Some(TaskKindTag::Researching)
    } else if contains_any(&lower, &["summary", "summarize", "tldr", "tl;dr"]) {
        Some(TaskKindTag::Summarizing)
    } else if contains_any(&lower, &["edit", "revise", "improve the writing"]) {
        Some(TaskKindTag::Editing)
    } else if contains_any(&lower, &["outreach", "lead", "cold email", "follow-up"]) {
        Some(TaskKindTag::Outreach)
    } else if contains_any(&lower, &["analyze", "data", "metrics", "trend"]) {
        Some(TaskKindTag::DataAnalysis)
    } else if contains_any(&lower, &["triage", "sort", "categorize"]) {
        Some(TaskKindTag::Triage)
    } else {
        None
    };

    ClassifiedTask {
        domain,
        kind,
        language: None,
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// The router's verdict, persisted on the arc so the user can see *why*
/// a particular profile was chosen and override it if it's wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingDecision {
    pub profile_id: ProfileId,
    pub score: i32,
    /// Human-readable explanation, e.g. "matched on domain:Email,
    /// kind:Drafting". Shown verbatim in the UI sidebar.
    pub reason: String,
}

/// Score a single profile against a classified task. Higher = better fit.
///
/// Scoring weights:
/// - +3 per matching `DomainTag` (the strongest signal, since source-driven)
/// - +2 per matching `TaskKindTag`
/// - +1 per matching language
/// - -10 if the classified kind is in the profile's `avoid` list (hard
///   penalty; profiles can refuse work explicitly)
///
/// A profile that doesn't declare any matching expertise scores 0 and is
/// filtered out by `pick_profile` — the caller falls back to the default.
/// We deliberately don't add a "declared at all" bonus: that would let
/// profiles with broad expertise win tasks they have no specific signal for.
pub fn score_profile(classified: &ClassifiedTask, profile: &AgentProfile) -> i32 {
    let exp = &profile.expertise;
    let mut score = 0i32;

    if let Some(d) = classified.domain {
        if exp.domains.contains(&d) {
            score += 3;
        }
    }
    if let Some(k) = classified.kind {
        if exp.task_kinds.contains(&k) {
            score += 2;
        }
        if exp.avoid.contains(&k) {
            score -= 10;
        }
    }
    if let Some(lang) = &classified.language {
        let needle = lang.to_lowercase();
        if exp.languages.iter().any(|l| l.to_lowercase() == needle) {
            score += 1;
        }
    }

    score
}

/// Pick the best-scoring profile for a classified task. Returns `None` if
/// no profile scores positively — caller falls back to `AgentProfile::DEFAULT_ID`.
///
/// The seeded default profile is excluded from competition: it's the
/// fallback, not a candidate. Ties broken by profile id ascending so the
/// outcome is deterministic.
pub fn pick_profile(
    classified: &ClassifiedTask,
    profiles: &[AgentProfile],
) -> Option<RoutingDecision> {
    let mut scored: Vec<(i32, &AgentProfile)> = profiles
        .iter()
        .filter(|p| p.id != AgentProfile::DEFAULT_ID)
        .map(|p| (score_profile(classified, p), p))
        .filter(|(s, _)| *s > 0)
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.id.cmp(&b.1.id)));

    scored.first().map(|(score, profile)| {
        let mut reason_parts = Vec::new();
        if let Some(d) = classified.domain {
            if profile.expertise.domains.contains(&d) {
                reason_parts.push(format!("domain:{d:?}"));
            }
        }
        if let Some(k) = classified.kind {
            if profile.expertise.task_kinds.contains(&k) {
                reason_parts.push(format!("kind:{k:?}"));
            }
        }
        let reason = if reason_parts.is_empty() {
            "no specific match (catch-all)".to_string()
        } else {
            format!("matched on {}", reason_parts.join(", "))
        };
        RoutingDecision {
            profile_id: profile.id.clone(),
            score: *score,
            reason,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_profile::{ExpertiseDeclaration, ToolSelection};
    use chrono::Utc;

    fn make_profile(
        id: &str,
        domains: Vec<DomainTag>,
        task_kinds: Vec<TaskKindTag>,
        avoid: Vec<TaskKindTag>,
    ) -> AgentProfile {
        let now = Utc::now();
        AgentProfile {
            id: id.to_string(),
            display_name: id.to_string(),
            description: String::new(),
            persona_template_ids: vec![],
            custom_persona_addendum: None,
            tool_selection: ToolSelection::All,
            expertise: ExpertiseDeclaration {
                domains,
                task_kinds,
                avoid,
                ..Default::default()
            },
            model_profile_hint: None,
            builtin: false,
            created_at: now,
            updated_at: now,
        }
    }

    fn default_profile() -> AgentProfile {
        let now = Utc::now();
        AgentProfile {
            id: AgentProfile::DEFAULT_ID.to_string(),
            display_name: "Athen".into(),
            description: String::new(),
            persona_template_ids: vec![],
            custom_persona_addendum: None,
            tool_selection: ToolSelection::All,
            expertise: ExpertiseDeclaration::default(),
            model_profile_hint: None,
            builtin: true,
            created_at: now,
            updated_at: now,
        }
    }

    #[test]
    fn classify_email_source_tags_domain() {
        let c = classify_task(Some("email"), "Hello, please review.");
        assert_eq!(c.domain, Some(DomainTag::Email));
    }

    #[test]
    fn classify_user_input_no_source_tag() {
        let c = classify_task(Some("user_input"), "What's the weather?");
        assert_eq!(c.domain, None);
    }

    #[test]
    fn classify_picks_kind_from_keywords() {
        assert_eq!(
            classify_task(None, "Please draft a reply to Bob").kind,
            Some(TaskKindTag::Drafting)
        );
        assert_eq!(
            classify_task(None, "Schedule a meeting at 3pm").kind,
            Some(TaskKindTag::Scheduling)
        );
        assert_eq!(
            classify_task(None, "There's a stack trace I can't decipher").kind,
            Some(TaskKindTag::Debugging)
        );
        assert_eq!(
            classify_task(None, "Refactor this module").kind,
            Some(TaskKindTag::Coding)
        );
        assert_eq!(
            classify_task(None, "Cold email to a new lead").kind,
            Some(TaskKindTag::Outreach)
        );
    }

    #[test]
    fn classify_unknown_keeps_kind_none() {
        let c = classify_task(None, "the sky is blue today");
        assert_eq!(c.kind, None);
    }

    #[test]
    fn pick_profile_routes_email_drafting_to_outreach() {
        let outreach = make_profile(
            "outreach",
            vec![DomainTag::Email, DomainTag::Outreach],
            vec![TaskKindTag::Drafting, TaskKindTag::Outreach],
            vec![TaskKindTag::Coding],
        );
        let coder = make_profile(
            "coder",
            vec![DomainTag::Coding],
            vec![TaskKindTag::Coding, TaskKindTag::Debugging],
            vec![],
        );

        let classified = classify_task(Some("email"), "Draft a follow-up to a lead");
        let decision = pick_profile(&classified, &[outreach, coder, default_profile()]).unwrap();
        assert_eq!(decision.profile_id, "outreach");
        assert!(decision.reason.contains("domain:Email"));
        assert!(decision.reason.contains("kind:Drafting"));
    }

    #[test]
    fn pick_profile_avoid_penalty_demotes_match() {
        let avoidant = make_profile(
            "outreach",
            vec![DomainTag::Outreach],
            vec![TaskKindTag::Outreach],
            vec![TaskKindTag::Coding],
        );
        let coder = make_profile(
            "coder",
            vec![DomainTag::Coding],
            vec![TaskKindTag::Coding],
            vec![],
        );

        // A coding task. The outreach profile claims no domain/kind here, but
        // also doesn't avoid coding — so it scores 0 (filtered) while coder
        // wins with positive score.
        let classified = ClassifiedTask {
            domain: Some(DomainTag::Coding),
            kind: Some(TaskKindTag::Coding),
            language: None,
        };
        let decision = pick_profile(&classified, &[avoidant.clone(), coder.clone()]).unwrap();
        assert_eq!(decision.profile_id, "coder");

        // Now the outreach profile DOES claim coding (rare, but for the test):
        // the avoid penalty crushes its score even though kind matches.
        let avoidant_strong = make_profile(
            "outreach",
            vec![DomainTag::Coding],
            vec![TaskKindTag::Coding],
            vec![TaskKindTag::Coding],
        );
        let decision2 = pick_profile(&classified, &[avoidant_strong, coder]).unwrap();
        assert_eq!(decision2.profile_id, "coder");
    }

    #[test]
    fn pick_profile_returns_none_when_no_match() {
        // No declared profile claims this kind/domain, so caller falls back
        // to the seeded default.
        let candidate = make_profile(
            "outreach",
            vec![DomainTag::Outreach],
            vec![TaskKindTag::Outreach],
            vec![],
        );
        let classified = ClassifiedTask {
            domain: Some(DomainTag::Coding),
            kind: Some(TaskKindTag::Debugging),
            language: None,
        };
        assert!(pick_profile(&classified, &[candidate, default_profile()]).is_none());
    }

    #[test]
    fn pick_profile_excludes_default_from_competition() {
        // Even if the default profile somehow gets a positive score (it
        // can't with empty expertise, but defensively): it's never returned
        // — that's the caller's fallback target.
        let classified = ClassifiedTask {
            domain: Some(DomainTag::Email),
            kind: None,
            language: None,
        };
        let only_default = vec![default_profile()];
        assert!(pick_profile(&classified, &only_default).is_none());
    }

    #[test]
    fn pick_profile_breaks_ties_alphabetically() {
        let alpha = make_profile(
            "alpha",
            vec![DomainTag::Email],
            vec![TaskKindTag::Drafting],
            vec![],
        );
        let bravo = make_profile(
            "bravo",
            vec![DomainTag::Email],
            vec![TaskKindTag::Drafting],
            vec![],
        );
        let classified = ClassifiedTask {
            domain: Some(DomainTag::Email),
            kind: Some(TaskKindTag::Drafting),
            language: None,
        };
        let d = pick_profile(&classified, &[bravo, alpha]).unwrap();
        assert_eq!(d.profile_id, "alpha");
    }
}
