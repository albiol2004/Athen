//! Arc compaction trait — see `docs/ARC_COMPACTION.md` §7.
//!
//! The compactor is the executor's gateway into arc history. Direct reads of
//! `arc_entries` from the context-build path are forbidden — they bypass the
//! compaction view and reintroduce the unbounded-context bug.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Minimal, persistence-agnostic view of an arc entry. The persistence
/// layer converts `athen_persistence::arcs::ArcEntry` into this type so
/// `athen-core` does not depend on `athen-persistence` (hexagonal rule).
///
/// Keep this type lean — fields the compactor or executor actually needs,
/// nothing else.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextEntry {
    pub id: i64,
    /// Free-form role/source string mirroring `ArcEntry.source`
    /// ("user" / "assistant" / "system" / "tool" / channel name).
    pub source: String,
    pub content: String,
    /// String form of `EntryType` ("message", "tool_call", "summary", ...).
    /// Stored as a string so this crate stays independent of the
    /// persistence layer's enum.
    pub entry_type: String,
}

/// The view the executor consumes: a possibly-empty summary, the verbatim
/// tail after the summary, and the latest-per-tool-series cache.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArcContextView {
    /// The most recent compaction summary, if any.
    pub summary: Option<ContextEntry>,
    /// Verbatim entries after the summary (or all entries when no summary
    /// exists). Sorted by `id` ascending.
    pub tail: Vec<ContextEntry>,
    /// Latest successful tool-call result per distinct tool name, drawn
    /// from the compacted prefix. May be empty.
    pub tool_cache: Vec<ContextEntry>,
}

/// Outcome of a compaction pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionOutcome {
    /// `true` if a new summary was written; `false` if the call was a
    /// no-op (e.g. arc was already compact under the budget).
    pub compacted: bool,
    /// Largest `arc_entries.id` covered by the new summary. `None` if
    /// the call was a no-op.
    pub summarized_through_entry_id: Option<i64>,
    /// Estimated tokens before compaction.
    pub tokens_before: u32,
    /// Estimated tokens after compaction.
    pub tokens_after: u32,
}

/// The compactor port. Phase-1 implementation lives in `athen-app` and
/// drives an LLM through the §4 prompt; later phases swap in deterministic
/// or embedding-driven scorers behind the same trait.
///
/// Two thresholds carry the policy: `trigger_tokens` (when to fire
/// compaction) and `target_tokens` (what size to compact down to). These
/// come from the active provider's `compaction_trigger_pct` and
/// `compaction_target_pct` resolved against `context_window_tokens`. The
/// trait stays provider-agnostic by taking the resolved token counts, not
/// the model-config schema.
#[async_trait]
pub trait ArcCompactor: Send + Sync {
    /// Fire compaction iff the arc's total token estimate exceeds
    /// `trigger_tokens`. With hysteresis, `trigger_tokens > target_tokens`,
    /// so a single compaction pass leaves us comfortably under the
    /// trigger and avoids ping-pong.
    async fn should_compact(&self, arc_id: &str, trigger_tokens: u32) -> Result<bool>;

    /// Run a compaction pass. Idempotent: a no-op if the arc already
    /// fits within `target_tokens` (or has too few entries to be worth
    /// summarizing).
    ///
    /// Pass `target_tokens = 0` to **force** compaction — useful for
    /// user-triggered "compact now" commands. With target=0 the budget
    /// gate is always exceeded, so the call collapses material whenever
    /// there is enough of it to bother (the per-implementation
    /// "minimum entries" floor still applies; it prevents a single
    /// trailing turn being summarized into nothing).
    async fn compact(&self, arc_id: &str, target_tokens: u32) -> Result<CompactionOutcome>;

    /// Build the LLM context view for `arc_id`. Returns the latest summary
    /// (if any) plus the verbatim tail and tool-series cache. The
    /// implementation never silently drops the open action or the latest
    /// outbound state per channel.
    async fn load_context_view(&self, arc_id: &str) -> Result<ArcContextView>;

    /// Compaction-aware context build. The default runs `should_compact`
    /// → optional `compact` → `load_context_view`, which is the idiomatic
    /// entry point for the executor: one call yields the most up-to-date
    /// view, transparently triggering a summarization pass when the arc
    /// has crossed `trigger_tokens`.
    ///
    /// `compact` failures do not propagate — they are swallowed and the
    /// call falls through to `load_context_view` over the
    /// not-yet-compacted arc. Compaction is best-effort: a stale or
    /// missing summary degrades gracefully into "all entries verbatim,"
    /// never a hard failure on the dispatch path. Callers who want
    /// telemetry should call `should_compact` + `compact` separately and
    /// log the `CompactionOutcome` themselves (compactor implementations
    /// live in app-level crates that own logging policy).
    async fn prepare_context(
        &self,
        arc_id: &str,
        trigger_tokens: u32,
        target_tokens: u32,
    ) -> Result<ArcContextView> {
        if self.should_compact(arc_id, trigger_tokens).await? {
            // Best-effort: discard errors so a failed summarization can
            // never block dispatch.
            let _ = self.compact(arc_id, target_tokens).await;
        }
        self.load_context_view(arc_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity-check that the trait is dyn-compatible. If this compiles,
    /// `Box<dyn ArcCompactor>` works at every call site.
    struct Noop;

    #[async_trait]
    impl ArcCompactor for Noop {
        async fn should_compact(&self, _: &str, _: u32) -> Result<bool> {
            Ok(false)
        }

        async fn compact(&self, _: &str, _: u32) -> Result<CompactionOutcome> {
            Ok(CompactionOutcome {
                compacted: false,
                summarized_through_entry_id: None,
                tokens_before: 0,
                tokens_after: 0,
            })
        }

        async fn load_context_view(&self, _: &str) -> Result<ArcContextView> {
            Ok(ArcContextView::default())
        }
    }

    #[tokio::test]
    async fn trait_is_dyn_compatible() {
        let c: Box<dyn ArcCompactor> = Box::new(Noop);
        assert!(!c.should_compact("arc", 83_200).await.unwrap());
        let outcome = c.compact("arc", 38_400).await.unwrap();
        assert!(!outcome.compacted);
        let view = c.load_context_view("arc").await.unwrap();
        assert!(view.summary.is_none());
        assert!(view.tail.is_empty());
        assert!(view.tool_cache.is_empty());
    }
}
