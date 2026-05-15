//! Render the skill store's name+description listing into the markdown
//! block the executor splices into its system header.
//!
//! Sibling of `identity_render.rs` — same shape, different store. Bodies are
//! NOT included: this is the **discovery surface**, not the content surface.
//! The agent invokes `load_skill(slug)` to pull a body on demand.
//!
//! Output is profile-filtered: only skills whose `applies_to` matches the
//! active profile are listed. Ordered by `slug ASC` so the prompt prefix
//! cache stays valid across turns.

use std::sync::Arc;

use athen_core::traits::skill::SkillStore;
use athen_persistence::skills::SqliteSkillStore;

/// Render the skills listing for `profile_id`. Returns `None` when the
/// store is unwired or the active profile sees no skills — the executor's
/// system prompt is then byte-identical to today's.
///
/// Errors from the store are logged and swallowed: skills are enrichment,
/// not a hard requirement, and a SQLite hiccup must not block dispatch.
pub async fn render_skills_block(
    store: Option<&Arc<SqliteSkillStore>>,
    profile_id: &str,
) -> Option<String> {
    let store = store?;
    let skills = match store.list(Some(profile_id)).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("skills_render: list failed for '{profile_id}': {e}");
            return None;
        }
    };
    if skills.is_empty() {
        return None;
    }
    let mut out = String::new();
    for s in skills {
        // One line per skill: `- slug: description`. Description is
        // single-line by frontmatter convention; defensive trim+collapse
        // if a user wrote a multi-line one.
        let desc = s.description.replace('\n', " ");
        let desc = desc.trim();
        out.push_str("- ");
        out.push_str(&s.slug);
        out.push_str(": ");
        out.push_str(desc);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::identity::ProfileTag;
    use athen_core::skill::SkillFrontmatter;
    use athen_persistence::Database;
    use tempfile::TempDir;

    async fn fresh_store() -> (Arc<SqliteSkillStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        let db = Database::in_memory().await.unwrap();
        let store = Arc::new(db.skill_store(dir.path().to_path_buf()));
        (store, dir)
    }

    fn fm(name: &str, desc: &str, applies: Vec<ProfileTag>) -> SkillFrontmatter {
        SkillFrontmatter {
            name: name.into(),
            description: desc.into(),
            applies_to: applies,
        }
    }

    #[tokio::test]
    async fn empty_store_returns_none() {
        let (store, _dir) = fresh_store().await;
        assert!(render_skills_block(Some(&store), "default").await.is_none());
    }

    #[tokio::test]
    async fn unwired_store_returns_none() {
        assert!(render_skills_block(None, "default").await.is_none());
    }

    #[tokio::test]
    async fn lists_slug_and_description() {
        let (store, _dir) = fresh_store().await;
        store
            .upsert(
                "cold-email",
                &fm(
                    "cold-email",
                    "Use when drafting a cold email.",
                    vec![ProfileTag::Always],
                ),
                "body",
            )
            .await
            .unwrap();
        let out = render_skills_block(Some(&store), "default").await.unwrap();
        assert!(out.contains("- cold-email: Use when drafting a cold email."));
    }

    #[tokio::test]
    async fn skips_skills_not_applying_to_profile() {
        let (store, _dir) = fresh_store().await;
        store
            .upsert(
                "outreach-only",
                &fm("o", "o", vec![ProfileTag::Profile("outreach".into())]),
                "b",
            )
            .await
            .unwrap();
        // Wrong profile sees nothing → None.
        assert!(render_skills_block(Some(&store), "coder").await.is_none());
        let out = render_skills_block(Some(&store), "outreach").await.unwrap();
        assert!(out.contains("outreach-only"));
    }

    #[tokio::test]
    async fn order_is_slug_ascending() {
        let (store, _dir) = fresh_store().await;
        for slug in ["zeta", "alpha", "mu"] {
            store
                .upsert(slug, &fm(slug, "desc", vec![ProfileTag::Always]), "b")
                .await
                .unwrap();
        }
        let out = render_skills_block(Some(&store), "default").await.unwrap();
        let a = out.find("- alpha:").unwrap();
        let m = out.find("- mu:").unwrap();
        let z = out.find("- zeta:").unwrap();
        assert!(a < m && m < z);
    }

    #[tokio::test]
    async fn multiline_description_collapsed_to_single_line() {
        let (store, _dir) = fresh_store().await;
        store
            .upsert(
                "multi",
                &fm("m", "line one\nline two", vec![ProfileTag::Always]),
                "b",
            )
            .await
            .unwrap();
        let out = render_skills_block(Some(&store), "default").await.unwrap();
        // Should be a single line for the skill, no embedded newlines in the desc.
        let line_count = out.lines().count();
        assert_eq!(line_count, 1, "expected single-line listing, got: {out}");
    }
}
