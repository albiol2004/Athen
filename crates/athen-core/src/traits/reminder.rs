//! System-reminder builder — produces ephemeral per-turn re-anchoring text
//! that the executor injects into the message stream every few iterations
//! to fight tool-selection drift on long arcs.
//!
//! Unlike the static system prompt (cached, loaded once per LLM call),
//! reminders appear inline in the conversation. Recency bias gives them
//! disproportionate attention weight, so they're how we keep the "which
//! profile am I, what tools do I have, what are my hard rules?" contract
//! fresh past iteration 5–10 where lost-in-the-middle starts to degrade
//! tool-call quality.
//!
//! Reminders sit in the dynamic suffix and never invalidate the cached
//! static prefix — see `feedback_prompt_cache_optimization.md` for the
//! cache-discipline rules.

/// Per-call context the executor hands to the builder. Today only
/// `iteration` drives the decision; the other fields are placeholders so
/// the trait stays stable when we layer in Replit-style trajectory-aware
/// injection (repeated tool patterns, error sequences, …).
#[derive(Debug, Clone)]
pub struct ReminderContext<'a> {
    /// 0-indexed loop iteration. `0` is the very first LLM call, before
    /// any tools have run. Builders typically skip 0 — the static system
    /// prompt is fresh enough at that point.
    pub iteration: u32,
    /// Tool names dispatched in this run, in call order. Empty on
    /// iteration 0.
    pub tools_called: &'a [String],
    /// Names of tools that returned `success: false` since the last
    /// reminder fired. Hint for builders that want to escalate on errors.
    pub recent_failed_tools: &'a [String],
}

impl<'a> ReminderContext<'a> {
    /// Convenience for callers (mostly tests) that only need the
    /// iteration count.
    pub fn at(iteration: u32) -> Self {
        Self {
            iteration,
            tools_called: &[],
            recent_failed_tools: &[],
        }
    }
}

/// Builds ephemeral reminder content the executor splices into the
/// message stream between turns. Returning `None` means "skip injection
/// this iteration" — that's the default state for most turns since
/// constantly anchoring would bloat context.
///
/// Implementations must be cheap: `build` runs once per loop iteration.
/// Heavy distillation (template lookups, tool-list rendering, identity
/// excerpts) belongs in the constructor, not here.
///
/// The returned `String` is the reminder *body*; the executor wraps it
/// in `<system-reminder>...</system-reminder>` tags before pushing it
/// into the conversation, so impls must not include the tags themselves.
pub trait SystemReminderBuilder: Send + Sync {
    fn build(&self, ctx: &ReminderContext<'_>) -> Option<String>;
}
