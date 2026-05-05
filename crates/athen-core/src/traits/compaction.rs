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
/// Design note on the `model_window_tokens` / `target_tokens` parameter
/// pair: there is no shared `ModelId` type yet (model identity is split
/// across `ProviderConfig` and `ModelProfile`), and synthesising one purely
/// for this trait would couple compaction to the model-config schema. The
/// trait stays provider-agnostic by taking the two numbers it actually
/// needs — the model's authoritative context window and the per-arc
/// budget the caller has resolved from
/// `compaction_trigger_pct` / `compaction_target_pct`.
#[async_trait]
pub trait ArcCompactor: Send + Sync {
    /// Decide whether `arc_id` needs compaction under the given budget.
    async fn should_compact(
        &self,
        arc_id: &str,
        model_window_tokens: u32,
        target_tokens: u32,
    ) -> Result<bool>;

    /// Run a compaction pass. Idempotent: a no-op if the arc already fits.
    async fn compact(
        &self,
        arc_id: &str,
        model_window_tokens: u32,
        target_tokens: u32,
    ) -> Result<CompactionOutcome>;

    /// Build the LLM context view for `arc_id`. Returns the latest summary
    /// (if any) plus the verbatim tail and tool-series cache.
    ///
    /// `target_tokens` is advisory — the implementation may return a view
    /// slightly above or below it, but should never silently drop the open
    /// action or the latest outbound state per channel.
    async fn load_context_view(
        &self,
        arc_id: &str,
        model_window_tokens: u32,
        target_tokens: u32,
    ) -> Result<ArcContextView>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity-check that the trait is dyn-compatible. If this compiles,
    /// `Box<dyn ArcCompactor>` works at every call site.
    struct Noop;

    #[async_trait]
    impl ArcCompactor for Noop {
        async fn should_compact(&self, _: &str, _: u32, _: u32) -> Result<bool> {
            Ok(false)
        }

        async fn compact(&self, _: &str, _: u32, _: u32) -> Result<CompactionOutcome> {
            Ok(CompactionOutcome {
                compacted: false,
                summarized_through_entry_id: None,
                tokens_before: 0,
                tokens_after: 0,
            })
        }

        async fn load_context_view(&self, _: &str, _: u32, _: u32) -> Result<ArcContextView> {
            Ok(ArcContextView::default())
        }
    }

    #[tokio::test]
    async fn trait_is_dyn_compatible() {
        let c: Box<dyn ArcCompactor> = Box::new(Noop);
        assert!(!c.should_compact("arc", 128_000, 38_400).await.unwrap());
        let outcome = c.compact("arc", 128_000, 38_400).await.unwrap();
        assert!(!outcome.compacted);
        let view = c.load_context_view("arc", 128_000, 38_400).await.unwrap();
        assert!(view.summary.is_none());
        assert!(view.tail.is_empty());
        assert!(view.tool_cache.is_empty());
    }
}
