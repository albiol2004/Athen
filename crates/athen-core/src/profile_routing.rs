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

use std::collections::HashMap;

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
    let lower = description.to_lowercase();

    // Source channel wins for domain when present — email/calendar/messaging
    // are unambiguous structural signals. Otherwise we fall back to keyword
    // detection across the broader domain set.
    let domain = match source {
        Some("email") => Some(DomainTag::Email),
        Some("calendar") => Some(DomainTag::Calendar),
        Some("messaging") => Some(DomainTag::Messaging),
        _ => infer_domain_from_keywords(&lower),
    };

    let kind = if contains_any(&lower, &["draft", "write", "compose", "reply", "respond"]) {
        Some(TaskKindTag::Drafting)
    } else if contains_any(
        &lower,
        &["schedule", "meeting", "appointment", "book", "calendar"],
    ) {
        Some(TaskKindTag::Scheduling)
    } else if contains_any(&lower, &["debug", "fix the bug", "stack trace", "error in"]) {
        Some(TaskKindTag::Debugging)
    } else if contains_any(
        &lower,
        &["code", "implement", "function", "refactor", "module"],
    ) {
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

/// Keyword-driven domain inference for free-form descriptions (no source
/// channel). Order matters: more specific domains check first so that, e.g.,
/// "deploy a kubernetes pod" wins Infrastructure before the broader Coding
/// keyword `code` would steal it.
fn infer_domain_from_keywords(lower: &str) -> Option<DomainTag> {
    if contains_any(
        lower,
        &[
            "kubernetes",
            "k8s",
            "docker",
            "podman",
            "vercel",
            "supabase",
            "deploy",
            "deployment",
            "helm",
            "terraform",
            "ci/cd",
            "pipeline",
            "github actions",
            "gitlab ci",
            "ansible",
            "nginx",
            "load balancer",
            "kustomize",
            "rollout",
            "kubectl",
        ],
    ) {
        return Some(DomainTag::Infrastructure);
    }
    if contains_any(
        lower,
        &[
            "linkedin",
            "tiktok",
            "instagram",
            "twitter",
            "x.com",
            "social media",
            "hashtag",
            "reel",
            "story",
            "carousel",
            "creator",
            "post caption",
            "engagement rate",
            "follower",
        ],
    ) {
        return Some(DomainTag::SocialMedia);
    }
    if contains_any(
        lower,
        &[
            "legal",
            "lawyer",
            "attorney",
            "statute",
            "case law",
            "regulation",
            "gdpr",
            "ccpa",
            "hipaa",
            "contract clause",
            "lawsuit",
            "court",
            "compliance",
            "tort",
            "jurisdiction",
            "terms of service",
        ],
    ) {
        return Some(DomainTag::Legal);
    }
    if contains_any(
        lower,
        &[
            "symptom",
            "medication",
            "diagnosis",
            "medical",
            "illness",
            "treatment",
            "side effect",
            "dosage",
            "patient",
            "prescription",
            "clinical trial",
            "peer-reviewed",
            "peer reviewed",
        ],
    ) {
        return Some(DomainTag::Health);
    }
    if contains_any(
        lower,
        &[
            "architecture",
            "system design",
            "scalability",
            "microservice",
            "monolith",
            "data model",
            "service boundary",
            "design pattern",
            "high availability",
            "fault tolerance",
        ],
    ) {
        return Some(DomainTag::Architecture);
    }
    if contains_any(
        lower,
        &[
            "linux",
            "ubuntu",
            "fedora",
            "arch linux",
            "wsl",
            "systemd",
            "permission denied",
            "command not found",
            "package manager",
            "apt install",
            "dnf install",
            "pacman",
            "shell error",
            "troubleshoot",
            "won't start",
            "won't run",
            "broken environment",
        ],
    ) {
        return Some(DomainTag::Support);
    }
    if contains_any(
        lower,
        &[
            "outreach",
            "cold email",
            "prospect",
            "lead generation",
            "follow-up",
            "follow up email",
        ],
    ) {
        return Some(DomainTag::Outreach);
    }
    if contains_any(
        lower,
        &[
            "marketing",
            "conversion",
            "funnel",
            "ad copy",
            "landing page",
            "campaign",
            "ctr",
            "click-through",
            "ctas",
        ],
    ) {
        return Some(DomainTag::Marketing);
    }
    if contains_any(
        lower,
        &[
            "implement",
            "refactor",
            "function",
            "module",
            "stack trace",
            "bug fix",
            "debug",
            "compile",
            "type error",
            "unit test",
        ],
    ) {
        return Some(DomainTag::Coding);
    }
    if contains_any(
        lower,
        &[
            "research",
            "investigate",
            "look up",
            "find out",
            "what is",
            "what are",
        ],
    ) {
        return Some(DomainTag::Research);
    }
    None
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

// ---------------------------------------------------------------------------
// Semantic blending — embeddings as a second-pass refinement
// ---------------------------------------------------------------------------

/// The text the embedder should index for a profile. Concatenates the
/// fields that describe the profile's intent: name, one-line description,
/// strengths, and any custom persona addendum. We deliberately exclude
/// the closed-enum tags (domains/task_kinds) — those are already covered
/// by the keyword stage; embeddings shine on the free-form fields.
pub fn profile_embedding_text(profile: &AgentProfile) -> String {
    let mut parts = Vec::with_capacity(4);
    if !profile.display_name.is_empty() {
        parts.push(profile.display_name.clone());
    }
    if !profile.description.is_empty() {
        parts.push(profile.description.clone());
    }
    if !profile.expertise.strengths.is_empty() {
        parts.push(profile.expertise.strengths.join(", "));
    }
    if let Some(addendum) = &profile.custom_persona_addendum {
        if !addendum.is_empty() {
            parts.push(addendum.clone());
        }
    }
    parts.join("\n")
}

/// Cosine similarity between two equal-length vectors. Returns `0.0` if
/// the lengths disagree or either side is the zero vector — both are
/// real-world failure modes (a misconfigured embedder, a one-shot empty
/// description) we'd rather treat as "no signal" than panic on.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Multiplier applied to the cosine similarity before it's added to the
/// keyword score. The keyword pass produces small integer scores (typically
/// 0..6); we want semantic to nudge ties and promote close-but-untagged
/// matches without overwhelming an explicit domain match. A weight of 4
/// puts a "perfect cosine" semantic match on par with one keyword domain
/// match; a 0.5-cosine match adds 2 — enough to break ties.
const SEMANTIC_WEIGHT: f32 = 4.0;

/// Pick the best-scoring profile, blending keyword scoring with embedding
/// similarity when embeddings are available. Falls back to the keyword
/// path verbatim when `query_embedding` is `None`, when a profile has no
/// cached embedding, or when vectors don't line up.
///
/// Caller responsibilities (caching belongs at the call site so the cache
/// can outlive any single classification):
/// - Compute `query_embedding` from the task description (or pass `None`).
/// - Maintain `profile_embeddings`, keyed by `profile.id`. Missing entries
///   silently fall back to keyword-only for that profile.
///
/// Determinism: ties on the blended score break the same way as the
/// keyword path — by profile id ascending.
pub fn pick_profile_blended(
    classified: &ClassifiedTask,
    profiles: &[AgentProfile],
    query_embedding: Option<&[f32]>,
    profile_embeddings: &HashMap<ProfileId, Vec<f32>>,
) -> Option<RoutingDecision> {
    // First: keyword scoring. Profiles that score 0 on keywords AND have no
    // semantic backing get filtered out. A profile with only semantic
    // signal can still win — we don't require a positive keyword score
    // when embeddings push it past the threshold.
    let mut scored: Vec<(f32, &AgentProfile, i32, f32)> = profiles
        .iter()
        .filter(|p| p.id != AgentProfile::DEFAULT_ID)
        .map(|p| {
            let kw = score_profile(classified, p);
            let sem = match (query_embedding, profile_embeddings.get(&p.id)) {
                (Some(q), Some(pe)) => cosine_similarity(q, pe).max(0.0),
                _ => 0.0,
            };
            let blended = kw as f32 + sem * SEMANTIC_WEIGHT;
            (blended, p, kw, sem)
        })
        // Threshold: keep profiles with a positive keyword score OR a
        // non-trivial semantic hit. 0.55 cosine is the floor where a
        // semantic-only match is genuinely related; below that, the
        // multiplied score would still be positive but would route random
        // tasks to random profiles.
        .filter(|(_, _, kw, sem)| *kw > 0 || *sem > 0.55)
        .collect();

    scored.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.id.cmp(&b.1.id))
    });

    scored.first().map(|(blended, profile, kw, sem)| {
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
        if *sem > 0.0 {
            reason_parts.push(format!("semantic:{:.2}", *sem));
        }
        let reason = if reason_parts.is_empty() {
            "no specific match (catch-all)".to_string()
        } else {
            format!("matched on {}", reason_parts.join(", "))
        };
        RoutingDecision {
            profile_id: profile.id.clone(),
            // Round the float score to an int for display continuity with
            // the pure-keyword path. The internal blended value is what the
            // sort used; the integer is only for the user-facing log.
            score: blended.round() as i32,
            reason: format!("{reason} (kw={kw}, sem={sem:.2})"),
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

    // ─── Semantic blending ────────────────────────────────────────────

    #[test]
    fn cosine_similarity_handles_edge_cases() {
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!((cosine_similarity(&[1.0, 0.0], &[0.0, 1.0])).abs() < 1e-6);
    }

    #[test]
    fn profile_embedding_text_includes_signal_fields() {
        let mut p = make_profile(
            "social_media",
            vec![DomainTag::SocialMedia],
            vec![TaskKindTag::Drafting],
            vec![],
        );
        p.display_name = "Social Media Expert".into();
        p.description = "Platform-native posts.".into();
        p.expertise.strengths = vec!["linkedin".into(), "tiktok".into()];
        p.custom_persona_addendum = Some("Hooks first.".into());

        let text = profile_embedding_text(&p);
        assert!(text.contains("Social Media Expert"));
        assert!(text.contains("Platform-native posts."));
        assert!(text.contains("linkedin, tiktok"));
        assert!(text.contains("Hooks first."));
    }

    #[test]
    fn pick_profile_blended_falls_back_to_keywords_without_embeddings() {
        let outreach = make_profile(
            "outreach",
            vec![DomainTag::Outreach, DomainTag::Email],
            vec![TaskKindTag::Drafting],
            vec![],
        );
        let coder = make_profile(
            "coder",
            vec![DomainTag::Coding],
            vec![TaskKindTag::Coding],
            vec![],
        );
        let classified = classify_task(Some("email"), "Draft a follow-up");

        let no_embeddings: HashMap<ProfileId, Vec<f32>> = HashMap::new();
        let d =
            pick_profile_blended(&classified, &[outreach, coder], None, &no_embeddings).unwrap();
        assert_eq!(d.profile_id, "outreach");
    }

    #[test]
    fn pick_profile_blended_uses_semantic_to_break_ties() {
        // Both profiles have identical keyword scores. The semantic stage
        // gives outreach a stronger cosine match → it wins despite the
        // alphabetical tiebreak preferring "alpha".
        let alpha = make_profile(
            "alpha",
            vec![DomainTag::Email],
            vec![TaskKindTag::Drafting],
            vec![],
        );
        let outreach = make_profile(
            "outreach",
            vec![DomainTag::Email],
            vec![TaskKindTag::Drafting],
            vec![],
        );
        let classified = classify_task(Some("email"), "Draft a follow-up");

        let q = vec![1.0, 0.0, 0.0];
        let mut embeds = HashMap::new();
        embeds.insert("alpha".to_string(), vec![0.0, 1.0, 0.0]); // orthogonal
        embeds.insert("outreach".to_string(), vec![0.9, 0.1, 0.0]); // close

        let d = pick_profile_blended(&classified, &[alpha, outreach], Some(&q), &embeds).unwrap();
        assert_eq!(d.profile_id, "outreach");
        assert!(d.reason.contains("semantic:"));
    }

    #[test]
    fn pick_profile_blended_promotes_semantic_only_match_above_threshold() {
        // Profile claims no matching tags (keyword score 0) but its
        // embedding is very close to the query — it should still win.
        let mut social = make_profile("social_media", vec![], vec![], vec![]);
        social.expertise.strengths = vec!["linkedin posts".into()];

        let classified = classify_task(None, "write a thoughtful linkedin post about leadership");

        let q = vec![1.0, 0.0];
        let mut embeds = HashMap::new();
        // Strong cosine match (~0.99).
        embeds.insert("social_media".to_string(), vec![0.99, 0.05]);

        let d = pick_profile_blended(&classified, &[social], Some(&q), &embeds);
        assert!(
            d.is_some(),
            "semantic-only match above threshold should win"
        );
    }

    #[test]
    fn pick_profile_blended_rejects_weak_semantic_only_match() {
        // No keyword signal, weak semantic (below 0.55 threshold) → None.
        let candidate = make_profile("c", vec![], vec![], vec![]);
        let classified = classify_task(None, "completely unrelated query");
        let q = vec![1.0, 0.0];
        let mut embeds = HashMap::new();
        embeds.insert("c".to_string(), vec![0.3, 0.95]); // ~0.3 cosine
        let d = pick_profile_blended(&classified, &[candidate], Some(&q), &embeds);
        assert!(d.is_none());
    }
}
