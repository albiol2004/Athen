//! Render the identity store into the markdown block the executor splices
//! into its system header.
//!
//! The store is the source of truth (categories + entries with `applies_to`
//! tags); the executor wants a single profile-filtered string. This module
//! does the read + filter + format, called once per dispatch from the
//! command path.
//!
//! Format is intentionally minimal — `## category` headers separated by
//! blank lines, entry bodies inline (markdown verbatim, no escaping). The
//! executor wraps the result with `--- IDENTITY ---` framing so the LLM
//! can recognise it as a distinct contract from the agent persona above.

use std::sync::Arc;

use athen_core::traits::identity::IdentityStore;
use athen_persistence::identity::SqliteIdentityStore;

/// Render the identity block for `profile_id`. Returns `None` when the
/// store is unwired or the active profile has no matching entries — the
/// executor's system prompt is then byte-identical to today's.
///
/// Errors from the store are logged and swallowed: identity is enrichment,
/// not a hard requirement, and a SQLite hiccup must not block dispatch.
pub async fn render_identity_block(
    store: Option<&Arc<SqliteIdentityStore>>,
    profile_id: &str,
) -> Option<String> {
    let store = store?;
    let grouped = match store.entries_for_profile(profile_id).await {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!("identity_render: entries_for_profile failed for '{profile_id}': {e}");
            return None;
        }
    };
    if grouped.is_empty() {
        return None;
    }

    let mut out = String::new();
    for (cat, entries) in grouped {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("## ");
        out.push_str(&cat.name);
        out.push('\n');
        for entry in entries {
            // Body verbatim — markdown is allowed; trailing newline ensures
            // the next entry starts on its own line. Empty bodies are
            // tolerated (will render as a blank line) but the UI prevents
            // saving them.
            let body = entry.body.trim_end();
            out.push_str(body);
            out.push_str("\n\n");
        }
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::identity::{IdentityCategory, IdentityEntry, ProfileTag};
    use athen_persistence::Database;
    use chrono::Utc;
    use uuid::Uuid;

    async fn fresh_store() -> Arc<SqliteIdentityStore> {
        let db = Database::in_memory().await.unwrap();
        Arc::new(db.identity_store())
    }

    fn mk_entry(category: &str, body: &str, tags: Vec<ProfileTag>) -> IdentityEntry {
        let now = Utc::now();
        IdentityEntry {
            id: Uuid::new_v4(),
            category: category.into(),
            body: body.into(),
            applies_to: tags,
            pinned: false,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn empty_store_returns_none() {
        let store = fresh_store().await;
        let out = render_identity_block(Some(&store), "default").await;
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn unwired_store_returns_none() {
        let out = render_identity_block(None, "default").await;
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn renders_groups_in_sort_order() {
        let store = fresh_store().await;
        // personality (sort 10), rules (sort 20), knowledge (sort 30) seeds
        // are already there. Add one entry to each of personality and rules.
        store
            .upsert_entry(&mk_entry(
                "personality",
                "Be warm but concise.",
                vec![ProfileTag::Always],
            ))
            .await
            .unwrap();
        store
            .upsert_entry(&mk_entry(
                "rules",
                "Never auto-send to legal@.",
                vec![ProfileTag::Always],
            ))
            .await
            .unwrap();
        let out = render_identity_block(Some(&store), "default")
            .await
            .unwrap();
        // Personality header appears before rules header.
        let p = out.find("## personality").unwrap();
        let r = out.find("## rules").unwrap();
        assert!(p < r, "personality must precede rules in output");
        assert!(out.contains("Be warm but concise."));
        assert!(out.contains("Never auto-send to legal@."));
    }

    #[tokio::test]
    async fn skips_categories_without_matching_entries() {
        let store = fresh_store().await;
        // Only a coder-only entry. For a different profile, output must be
        // None (no category has a matching entry).
        store
            .upsert_entry(&mk_entry(
                "rules",
                "Prefer tracing.",
                vec![ProfileTag::Profile("coder".into())],
            ))
            .await
            .unwrap();
        assert!(render_identity_block(Some(&store), "assistant")
            .await
            .is_none());
        let coder = render_identity_block(Some(&store), "coder").await.unwrap();
        assert!(coder.contains("Prefer tracing."));
    }

    #[tokio::test]
    async fn user_added_category_renders_after_seeds() {
        let store = fresh_store().await;
        store
            .upsert_category(&IdentityCategory {
                name: "coding_style".into(),
                description: "Coding prefs".into(),
                default_applies_to: vec![ProfileTag::Profile("coder".into())],
                sort_order: 100,
                is_seed: false,
            })
            .await
            .unwrap();
        store
            .upsert_entry(&mk_entry(
                "personality",
                "Voice: terse.",
                vec![ProfileTag::Always],
            ))
            .await
            .unwrap();
        store
            .upsert_entry(&mk_entry(
                "coding_style",
                "Use tracing not println.",
                vec![ProfileTag::Profile("coder".into())],
            ))
            .await
            .unwrap();
        let out = render_identity_block(Some(&store), "coder").await.unwrap();
        let p = out.find("## personality").unwrap();
        let c = out.find("## coding_style").unwrap();
        assert!(
            p < c,
            "user category sort_order=100 must follow personality (10)"
        );
    }
}
