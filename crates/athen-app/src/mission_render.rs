//! Render the per-task MISSION block for the static prompt prefix.
//!
//! Reads the arc's persisted `triage_plan` (set once at task start via
//! `set_triage_plan_if_absent`) and formats it as a short, model-friendly
//! "Done when: … / Not in scope: …" block. When a user-set goal is
//! active on the arc, it takes precedence and is rendered first; the
//! triage plan (if any) follows.  The executor splices the result
//! between identity and workspace rules — see
//! `DefaultExecutor::build_mission_section`.
//!
//! Returns `None` (and the prompt block is omitted byte-for-byte) when:
//! - no `arc_store` is configured (test / CLI flows without persistence),
//! - the arc is missing or not yet created,
//! - the arc has no user goal AND no triage plan persisted.
//!
//! Companion to `identity_render`, `endpoints_render`, `skills_render`.

use athen_persistence::arcs::ArcStore;

/// Format the mission body — user-set goal (if active) first, then the
/// auto-drafted triage plan (if any).  The executor's
/// `build_mission_section` wraps the result in framing markers.
///
/// Returns `None` if the store is missing, the arc isn't found, or the
/// arc has neither an active user goal nor a triage plan.  Errors
/// reading the store are logged and treated as "no content" — degrading
/// gracefully is better than failing a turn that would otherwise run
/// fine without the MISSION block.
pub async fn render_mission_block(arc_store: Option<&ArcStore>, arc_id: &str) -> Option<String> {
    let store = arc_store?;
    let meta = match store.get_arc(arc_id).await {
        Ok(meta) => meta?,
        Err(e) => {
            tracing::warn!(arc = %arc_id, error = %e, "render_mission_block: get_arc failed");
            return None;
        }
    };

    let mut body = String::new();

    // User-set goal takes precedence over auto-drafted triage plan.
    if let Some(ref goal) = meta.user_goal {
        if meta.goal_status.as_deref() == Some("active") {
            body.push_str("GOAL (user-set): ");
            body.push_str(goal.trim());
            body.push('\n');
            if let Some(ref criteria) = meta.user_goal_criteria {
                let c = criteria.trim();
                if !c.is_empty() {
                    body.push_str("Done when: ");
                    body.push_str(c);
                    body.push('\n');
                }
            }
        }
    }

    // Existing triage plan follows below (if any).
    if let Some(plan) = meta.triage_plan {
        let acceptance = plan.acceptance_criteria.trim();
        let scope = plan.scope.trim();
        if !acceptance.is_empty() {
            // When a user goal already emitted its own "Done when:", skip
            // the triage plan's acceptance line to avoid duplication.
            if body.is_empty() || !body.contains("Done when: ") {
                body.push_str("Done when: ");
                body.push_str(acceptance);
                body.push('\n');
            }
        }
        if !scope.is_empty() {
            body.push_str("Not in scope: ");
            body.push_str(scope);
            body.push('\n');
        }
    }

    if body.is_empty() {
        return None;
    }
    Some(body)
}

/// Read the acceptance criteria for the completion judge.  When a
/// user-set goal is active, its criteria (or the goal text itself as
/// fallback) override the triage plan's `acceptance_criteria`.  Falls
/// back to the triage plan when no user goal is active.
///
/// Returns `None` (and the judge falls back to its historical
/// mismatch-only behavior) for the same set of cases as
/// `render_mission_block`: no store, missing arc, no plan/goal, empty
/// acceptance text.
pub async fn read_acceptance_criteria(
    arc_store: Option<&ArcStore>,
    arc_id: &str,
) -> Option<String> {
    let store = arc_store?;
    let meta = match store.get_arc(arc_id).await {
        Ok(meta) => meta?,
        Err(e) => {
            tracing::warn!(arc = %arc_id, error = %e, "read_acceptance_criteria: get_arc failed");
            return None;
        }
    };

    // User goal overrides triage plan for the completion judge.
    if meta.goal_status.as_deref() == Some("active") {
        if let Some(ref criteria) = meta.user_goal_criteria {
            let trimmed = criteria.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(ref goal) = meta.user_goal {
            let trimmed = goal.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    // Fall back to triage plan.
    let plan = meta.triage_plan?;
    let trimmed = plan.acceptance_criteria.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Read the goal status and optional blocked reason for the arc.
/// Used by executor-building code to decide `goal_mode`.
///
/// Returns `None` if the store is missing, the arc isn't found, or the
/// arc has no `goal_status` set.
pub async fn read_goal_status(
    arc_store: Option<&ArcStore>,
    arc_id: &str,
) -> Option<(String, Option<String>)> {
    let store = arc_store?;
    let meta = match store.get_arc(arc_id).await {
        Ok(meta) => meta?,
        Err(e) => {
            tracing::warn!(arc = %arc_id, error = %e, "read_goal_status: get_arc failed");
            return None;
        }
    };
    let status = meta.goal_status?;
    Some((status, meta.goal_blocked_reason.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::risk::TriagePlan;
    use athen_persistence::arcs::ArcSource;
    use rusqlite::Connection;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn setup() -> ArcStore {
        let conn = Arc::new(Mutex::new(Connection::open_in_memory().unwrap()));
        let store = ArcStore::new(conn);
        store.init_schema().await.unwrap();
        store
    }

    #[tokio::test]
    async fn returns_none_without_store() {
        assert!(render_mission_block(None, "arc1").await.is_none());
    }

    #[tokio::test]
    async fn returns_none_when_arc_missing() {
        let store = setup().await;
        assert!(render_mission_block(Some(&store), "ghost").await.is_none());
    }

    #[tokio::test]
    async fn returns_none_when_no_plan() {
        let store = setup().await;
        store
            .create_arc("arc1", "noplan", ArcSource::UserInput)
            .await
            .unwrap();
        assert!(render_mission_block(Some(&store), "arc1").await.is_none());
    }

    #[tokio::test]
    async fn acceptance_criteria_round_trip() {
        let store = setup().await;
        store
            .create_arc("arc_a", "withplan", ArcSource::UserInput)
            .await
            .unwrap();
        let plan = TriagePlan {
            acceptance_criteria: "Reply once with Q3 terms confirmed.".to_string(),
            scope: "NOT a multi-message thread.".to_string(),
        };
        store.set_triage_plan("arc_a", Some(&plan)).await.unwrap();

        let c = read_acceptance_criteria(Some(&store), "arc_a")
            .await
            .unwrap();
        assert_eq!(c, "Reply once with Q3 terms confirmed.");
    }

    #[tokio::test]
    async fn acceptance_criteria_none_without_plan() {
        let store = setup().await;
        store
            .create_arc("arc_b", "noplan", ArcSource::UserInput)
            .await
            .unwrap();
        assert!(read_acceptance_criteria(Some(&store), "arc_b")
            .await
            .is_none());
        assert!(read_acceptance_criteria(None, "arc_b").await.is_none());
    }

    #[tokio::test]
    async fn renders_done_when_and_not_in_scope() {
        let store = setup().await;
        store
            .create_arc("arc1", "withplan", ArcSource::UserInput)
            .await
            .unwrap();
        let plan = TriagePlan {
            acceptance_criteria: "Reply to João confirming Q3 terms.".to_string(),
            scope: "NOT a multi-message thread.".to_string(),
        };
        store.set_triage_plan("arc1", Some(&plan)).await.unwrap();

        let block = render_mission_block(Some(&store), "arc1").await.unwrap();
        assert!(block.contains("Done when: Reply to João confirming Q3 terms."));
        assert!(block.contains("Not in scope: NOT a multi-message thread."));
        // No framing — the executor's `build_mission_section` adds it.
        assert!(!block.contains("--- MISSION"));
    }
}
