//! Default `SystemReminderBuilder` impl.
//!
//! Distills an active agent profile + its filtered tool surface + identity
//! excerpt into a short reminder string at construction time. `build()`
//! returns the same pre-baked body every Nth iteration so per-turn cost is
//! a single map lookup + clone.
//!
//! Reminder shape (~90–180 tokens depending on profile breadth):
//!
//! ```text
//! Profile: Outreach Agent.
//! Tools available now: calendar_create, contacts_search, email_send, ...
//! Hard rules:
//! - Prefer dedicated tools over shell_execute when one fits.
//! - Use http_request only for endpoints listed in REGISTERED CLOUD APIs.
//! - Do NOT narrate intent; call the tool. Change approach if a tool fails twice.
//! Persona: <constraints excerpt>
//! Identity: <user-identity excerpt>
//! ```
//!
//! The body is intentionally short and concrete — long preambles defeat
//! the point of the reminder, which is to *re-anchor*, not to re-teach.
//! Persona / identity excerpts are clipped to ~240 chars each so a
//! verbose profile or large identity store can't blow the budget.

use athen_core::agent_profile::{PersonaCategory, ResolvedAgentProfile};
use athen_core::tool::ToolDefinition;
use athen_core::traits::reminder::{ReminderContext, SystemReminderBuilder};

/// Default reminder period. Fires on iterations 3, 6, 9, … — every Nth
/// LLM call after the first batch. Chosen so a typical 5–8 tool-call
/// task gets one or two re-anchors: enough to fight mid-run drift
/// without ballooning tokens.
pub const REMINDER_EVERY_N_ITERATIONS: u32 = 3;

/// Cap the tool-name list so a 30-tool registry doesn't ship a 600-byte
/// list every reminder.
const MAX_TOOLS_IN_REMINDER: usize = 24;
const MAX_PERSONA_EXCERPT_CHARS: usize = 240;
const MAX_IDENTITY_EXCERPT_CHARS: usize = 240;

/// Built once per executor with the resolved profile + post-filter tool
/// list + (optional) identity excerpt. `build()` returns the same body
/// for every qualifying iteration — no per-turn work.
pub struct ProfileSystemReminderBuilder {
    body: String,
    period: u32,
}

impl ProfileSystemReminderBuilder {
    /// Construct with the default firing period
    /// (`REMINDER_EVERY_N_ITERATIONS`). `tools` should be the
    /// post-`ToolSelection`-filter list the executor will actually expose
    /// — feeding the full registry would lie to the agent about what it
    /// can call.
    pub fn new(
        profile: Option<&ResolvedAgentProfile>,
        tools: &[ToolDefinition],
        identity_block: Option<&str>,
    ) -> Self {
        Self::with_period(profile, tools, identity_block, REMINDER_EVERY_N_ITERATIONS)
    }

    pub fn with_period(
        profile: Option<&ResolvedAgentProfile>,
        tools: &[ToolDefinition],
        identity_block: Option<&str>,
        period: u32,
    ) -> Self {
        let mut body = String::new();
        body.push_str("Profile: ");
        body.push_str(profile_display_name(profile));
        body.push_str(".\n");

        if !tools.is_empty() {
            body.push_str("Tools you can call right now: ");
            let mut names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
            names.sort_unstable();
            let truncated = names.len() > MAX_TOOLS_IN_REMINDER;
            if truncated {
                names.truncate(MAX_TOOLS_IN_REMINDER);
            }
            body.push_str(&names.join(", "));
            if truncated {
                body.push_str(", …");
            }
            body.push_str(".\n");
        }

        body.push_str(
            "Hard rules:\n\
             - Prefer dedicated tools over shell_execute when one fits (read, edit, write, list_directory).\n\
             - Use http_request only for endpoints listed in REGISTERED CLOUD APIs — do NOT install SDKs for those.\n\
             - Do NOT narrate intent; call the tool. If a tool failed twice with the same args, change approach.\n",
        );

        if let Some(excerpt) = persona_excerpt(profile) {
            body.push_str("Persona: ");
            body.push_str(&excerpt);
            body.push('\n');
        }

        if let Some(excerpt) = identity_excerpt(identity_block) {
            body.push_str("Identity: ");
            body.push_str(&excerpt);
            body.push('\n');
        }

        Self {
            body,
            period: period.max(1),
        }
    }

    /// Pre-rendered reminder body. Exposed for tests + the static-prefix
    /// estimator so the UI can preview reminder cost up-front.
    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn period(&self) -> u32 {
        self.period
    }
}

impl SystemReminderBuilder for ProfileSystemReminderBuilder {
    fn build(&self, ctx: &ReminderContext<'_>) -> Option<String> {
        if ctx.iteration == 0 || !ctx.iteration.is_multiple_of(self.period) {
            return None;
        }
        Some(self.body.clone())
    }
}

/// Wrap a reminder body in the framing tags the executor injects. Lives
/// here (not in the executor) so the framing stays close to the builder
/// — anyone adding a new builder grep-finds the wrapper in the same file.
pub fn wrap_reminder(body: &str) -> String {
    let trimmed = body.trim_end_matches('\n');
    format!("<system-reminder>\n{trimmed}\n</system-reminder>")
}

fn profile_display_name(profile: Option<&ResolvedAgentProfile>) -> &str {
    match profile {
        Some(p) if !p.profile.display_name.is_empty() => &p.profile.display_name,
        _ => "default Athen",
    }
}

fn persona_excerpt(profile: Option<&ResolvedAgentProfile>) -> Option<String> {
    let p = profile?;
    let mut s = String::new();
    // Constraints first — they're rule-shaped and most relevant for
    // re-anchoring "what am I forbidden from doing right now". Fall
    // through to Voice if no Constraints templates apply.
    for t in &p.persona_templates {
        if matches!(t.category, PersonaCategory::Constraints) {
            push_clipped(&mut s, &t.body, MAX_PERSONA_EXCERPT_CHARS);
            if s.chars().count() >= MAX_PERSONA_EXCERPT_CHARS {
                break;
            }
        }
    }
    if s.is_empty() {
        for t in &p.persona_templates {
            if matches!(t.category, PersonaCategory::Voice) {
                push_clipped(&mut s, &t.body, MAX_PERSONA_EXCERPT_CHARS);
                if s.chars().count() >= MAX_PERSONA_EXCERPT_CHARS {
                    break;
                }
            }
        }
    }
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn identity_excerpt(block: Option<&str>) -> Option<String> {
    let block = block?.trim();
    if block.is_empty() {
        return None;
    }
    let mut out = String::new();
    for line in block.lines() {
        let trimmed = line.trim();
        // Skip blank lines and the section dividers `build_identity_section`
        // emits ("--- IDENTITY ---" framing).
        if trimmed.is_empty() || trimmed.starts_with("---") {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(trimmed);
        if out.chars().count() >= MAX_IDENTITY_EXCERPT_CHARS {
            out = clip_chars(&out, MAX_IDENTITY_EXCERPT_CHARS);
            out.push('…');
            break;
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn push_clipped(dst: &mut String, src: &str, cap: usize) {
    let trimmed = src.trim();
    if trimmed.is_empty() {
        return;
    }
    if !dst.is_empty() {
        dst.push(' ');
    }
    if trimmed.chars().count() > cap {
        dst.push_str(&clip_chars(trimmed, cap));
        dst.push('…');
    } else {
        dst.push_str(trimmed);
    }
}

/// UTF-8-safe char-count clip. Avoids byte-slicing in the middle of a
/// multi-byte codepoint (panic risk for any identity entry with emoji,
/// accented chars, CJK, etc.).
fn clip_chars(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 4));
    for (i, c) in s.chars().enumerate() {
        if i >= max_chars {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::agent_profile::{
        AgentProfile, ExpertiseDeclaration, PersonaTemplate, ToolSelection,
    };
    use athen_core::tool::ToolDefinition;
    use chrono::Utc;

    fn td(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            description: format!("desc {name}"),
            parameters: serde_json::json!({"type": "object"}),
            backend: athen_core::tool::ToolBackend::Shell {
                command: String::new(),
                native: false,
            },
            base_risk: athen_core::risk::BaseImpact::Read,
        }
    }

    fn make_profile(display: &str, templates: Vec<PersonaTemplate>) -> ResolvedAgentProfile {
        let now = Utc::now();
        ResolvedAgentProfile {
            profile: AgentProfile {
                id: "p".into(),
                display_name: display.into(),
                description: "".into(),
                persona_template_ids: templates.iter().map(|t| t.id.clone()).collect(),
                custom_persona_addendum: None,
                tool_selection: ToolSelection::All,
                primary_groups: vec![],
                expertise: ExpertiseDeclaration::default(),
                model_profile_hint: None,
                builtin: false,
                created_at: now,
                updated_at: now,
            },
            persona_templates: templates,
        }
    }

    fn template(id: &str, category: PersonaCategory, body: &str) -> PersonaTemplate {
        PersonaTemplate {
            id: id.into(),
            display_name: id.into(),
            category,
            body: body.into(),
            builtin: false,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn skips_iteration_zero() {
        let b = ProfileSystemReminderBuilder::new(None, &[], None);
        assert!(b.build(&ReminderContext::at(0)).is_none());
    }

    #[test]
    fn fires_on_period_multiples_after_zero() {
        let b = ProfileSystemReminderBuilder::with_period(None, &[], None, 3);
        assert!(b.build(&ReminderContext::at(1)).is_none());
        assert!(b.build(&ReminderContext::at(2)).is_none());
        assert!(b.build(&ReminderContext::at(3)).is_some());
        assert!(b.build(&ReminderContext::at(4)).is_none());
        assert!(b.build(&ReminderContext::at(6)).is_some());
    }

    #[test]
    fn period_zero_is_clamped_to_one() {
        let b = ProfileSystemReminderBuilder::with_period(None, &[], None, 0);
        assert_eq!(b.period(), 1);
        // Still skips 0, fires on 1, 2, 3, ...
        assert!(b.build(&ReminderContext::at(0)).is_none());
        assert!(b.build(&ReminderContext::at(1)).is_some());
        assert!(b.build(&ReminderContext::at(2)).is_some());
    }

    #[test]
    fn body_contains_profile_name_and_tools() {
        let profile = make_profile("Outreach Agent", vec![]);
        let tools = vec![td("calendar_create"), td("email_send")];
        let b = ProfileSystemReminderBuilder::new(Some(&profile), &tools, None);
        let body = b.body();
        assert!(body.contains("Outreach Agent"));
        assert!(body.contains("calendar_create"));
        assert!(body.contains("email_send"));
        assert!(body.contains("Hard rules"));
    }

    #[test]
    fn default_profile_name_when_missing() {
        let b = ProfileSystemReminderBuilder::new(None, &[], None);
        assert!(b.body().contains("default Athen"));
    }

    #[test]
    fn tool_list_truncates_past_cap() {
        let tools: Vec<ToolDefinition> = (0..40).map(|i| td(&format!("tool_{i:02}"))).collect();
        let b = ProfileSystemReminderBuilder::new(None, &tools, None);
        assert!(b.body().contains("…"));
        // Tools after the cap should be absent.
        assert!(!b.body().contains("tool_39"));
        assert!(b.body().contains("tool_00"));
    }

    #[test]
    fn constraints_template_wins_over_voice() {
        let profile = make_profile(
            "p",
            vec![
                template("v", PersonaCategory::Voice, "speak softly"),
                template("c", PersonaCategory::Constraints, "never disclose secrets"),
            ],
        );
        let b = ProfileSystemReminderBuilder::new(Some(&profile), &[], None);
        let body = b.body();
        assert!(body.contains("never disclose secrets"));
        assert!(!body.contains("speak softly"));
    }

    #[test]
    fn falls_back_to_voice_when_no_constraints() {
        let profile = make_profile(
            "p",
            vec![template("v", PersonaCategory::Voice, "speak softly")],
        );
        let b = ProfileSystemReminderBuilder::new(Some(&profile), &[], None);
        assert!(b.body().contains("speak softly"));
    }

    #[test]
    fn identity_excerpt_skips_dividers_and_blanks() {
        let block = "--- IDENTITY ---\n\nuser is Alex\n\nworks on Athen\n--- END ---";
        let b = ProfileSystemReminderBuilder::new(None, &[], Some(block));
        let body = b.body();
        assert!(body.contains("user is Alex"));
        assert!(body.contains("works on Athen"));
        assert!(!body.contains("---"));
    }

    #[test]
    fn identity_excerpt_clips_at_char_cap_utf8_safe() {
        // String with multi-byte characters that would panic byte-slicing
        // at non-codepoint boundaries.
        let block = "café ".repeat(200); // 5 chars each, well past cap
        let b = ProfileSystemReminderBuilder::new(None, &[], Some(&block));
        // Should not panic; body should be bounded.
        assert!(b.body().contains("Identity:"));
        assert!(b.body().contains("…"));
    }

    #[test]
    fn wrap_reminder_adds_tags_and_trims_trailing_newlines() {
        let wrapped = wrap_reminder("hello\n\n");
        assert!(wrapped.starts_with("<system-reminder>\n"));
        assert!(wrapped.ends_with("</system-reminder>"));
        assert!(wrapped.contains("hello"));
        // No empty newline immediately before the closing tag.
        assert!(!wrapped.contains("\n\n</system-reminder>"));
    }

    #[test]
    fn empty_identity_yields_no_identity_line() {
        let b = ProfileSystemReminderBuilder::new(None, &[], Some("   \n  \n"));
        assert!(!b.body().contains("Identity:"));
    }

    #[test]
    fn build_returns_same_body_each_fire() {
        let b = ProfileSystemReminderBuilder::with_period(None, &[td("a")], None, 2);
        let a = b.build(&ReminderContext::at(2)).unwrap();
        let c = b.build(&ReminderContext::at(4)).unwrap();
        assert_eq!(a, c);
    }
}
