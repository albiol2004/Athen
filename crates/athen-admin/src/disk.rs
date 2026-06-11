//! Disk usage sweep + quota enforcement.
//!
//! Docker's hard disk quota (`storage_opt: size=`) only works when the
//! storage driver sits on xfs with project quotas — on every other setup
//! (including stock Fedora/Ubuntu hosts) container creation fails
//! outright. So quotas are enforced panel-side: a periodic `docker system
//! df` sweep measures each instance's data-volume usage, the dashboard
//! shows it, and crossing the per-instance `disk_limit_mb` escalates:
//!
//! 1. **first sweep over** — warn (audit row + webhook push), nothing
//!    stopped yet: the user gets one sweep interval to clean up;
//! 2. **next sweep still over** — the container is STOPPED (audit
//!    `disk_quota_stopped` + push). Set `ATHEN_ADMIN_DISK_ENFORCE=warn`
//!    to keep the old warn-only behavior.
//! 3. **restarted while still over** — one fresh sweep of grace (so the
//!    user can run the agent to delete files), then stopped again. The
//!    way out is shrinking the volume or raising the limit
//!    (`POST /panel/instances/{id}/disk_limit`).
//!
//! The flag re-arms only after usage falls back under 90% of the limit,
//! so a volume hovering at the threshold doesn't spam phones.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::db::{self, Instance};
use crate::{instances, notify, PanelState};

/// `docker system df` walks every volume — cheap at panel scale (tens of
/// volumes), but not something to hammer. Usage display tolerates 5 min
/// of staleness. Override with `ATHEN_ADMIN_DISK_SWEEP_SECS` (min 5).
const SWEEP_INTERVAL_SECS: u64 = 300;

/// Warning re-arms only after usage falls below this fraction of the
/// limit (hysteresis against threshold-hovering volumes).
const REARM_FRACTION: f64 = 0.9;

/// Per-instance escalation state while over quota.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum QuotaState {
    /// Warned; will be stopped if still over on the next sweep.
    Warned,
    /// Stopped by the panel for being over quota.
    Stopped,
}

/// What one sweep decided for one instance.
#[derive(PartialEq, Eq, Debug)]
enum QuotaAction {
    None,
    Warn,
    Stop,
}

/// Pure escalation step: previous state + this sweep's observations →
/// next state + action. `rearmed` means usage fell under the hysteresis
/// band; `running` is the container's live state; `enforce` is false in
/// warn-only mode.
fn quota_step(
    state: Option<QuotaState>,
    over: bool,
    rearmed: bool,
    running: bool,
    enforce: bool,
) -> (Option<QuotaState>, QuotaAction) {
    if !over {
        // Note: after a limit raise, a stale flag can linger in the
        // 90–100% band; crossing again then skips the warn grace. That's
        // deliberate — the user was already warned at this usage level.
        return (if rearmed { None } else { state }, QuotaAction::None);
    }
    match state {
        None => (Some(QuotaState::Warned), QuotaAction::Warn),
        Some(QuotaState::Warned) if enforce && running => {
            (Some(QuotaState::Stopped), QuotaAction::Stop)
        }
        Some(QuotaState::Warned) => (Some(QuotaState::Warned), QuotaAction::None),
        // Someone started it back up while still over: one sweep of
        // cleanup grace, then the Warned arm stops it again.
        Some(QuotaState::Stopped) if running => (Some(QuotaState::Warned), QuotaAction::None),
        Some(QuotaState::Stopped) => (Some(QuotaState::Stopped), QuotaAction::None),
    }
}

fn enforce_enabled() -> bool {
    !std::env::var("ATHEN_ADMIN_DISK_ENFORCE").is_ok_and(|v| v.eq_ignore_ascii_case("warn"))
}

fn sweep_interval() -> Duration {
    let secs = std::env::var("ATHEN_ADMIN_DISK_SWEEP_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(SWEEP_INTERVAL_SECS)
        .max(5);
    Duration::from_secs(secs)
}

/// Spawn the disk sweep loop. Returns immediately.
pub fn spawn(state: Arc<PanelState>) {
    tokio::spawn(run(state));
}

async fn run(state: Arc<PanelState>) {
    let enforce = enforce_enabled();
    let interval = sweep_interval();
    if !enforce {
        tracing::info!("disk quotas in warn-only mode (ATHEN_ADMIN_DISK_ENFORCE=warn)");
    }
    let mut flags: HashMap<String, QuotaState> = HashMap::new();
    loop {
        if let Err(e) = sweep(&state, &mut flags, enforce).await {
            tracing::debug!(error = format!("{e:#}"), "disk sweep failed (docker down?)");
        }
        tokio::time::sleep(interval).await;
    }
}

async fn sweep(
    state: &Arc<PanelState>,
    flags: &mut HashMap<String, QuotaState>,
    enforce: bool,
) -> anyhow::Result<()> {
    let usage = state.docker.volume_usage_bytes().await?;
    let status = state.docker.status_by_container().await?;
    let all = instances::list_all(&state.db).await?;
    let mut snapshot: HashMap<String, u64> = HashMap::new();
    for instance in all {
        let Some(bytes) = usage.get(&instance.volume_name).copied() else {
            continue; // volume gone or size unknown (non-local driver)
        };
        snapshot.insert(instance.id.clone(), bytes);
        let running = status
            .get(&instance.container_name)
            .is_some_and(|(s, _)| s == "running");
        apply_quota(state, &instance, bytes, running, enforce, flags).await;
    }
    *state.disk_usage.lock().expect("disk usage mutex poisoned") = snapshot;
    Ok(())
}

async fn apply_quota(
    state: &Arc<PanelState>,
    instance: &Instance,
    used_bytes: u64,
    running: bool,
    enforce: bool,
    flags: &mut HashMap<String, QuotaState>,
) {
    let Some(limit_mb) = instance.disk_limit_mb else {
        flags.remove(&instance.id);
        return;
    };
    let limit_bytes = limit_mb.saturating_mul(1024 * 1024);
    let used_mb = used_bytes / (1024 * 1024);
    let over = used_bytes > limit_bytes;
    let rearmed = (used_bytes as f64) < limit_bytes as f64 * REARM_FRACTION;
    let (next, action) = quota_step(
        flags.get(&instance.id).copied(),
        over,
        rearmed,
        running,
        enforce,
    );
    match next {
        Some(s) => {
            flags.insert(instance.id.clone(), s);
        }
        None => {
            flags.remove(&instance.id);
        }
    }
    match action {
        QuotaAction::None => {}
        QuotaAction::Warn => {
            tracing::warn!(
                instance = %instance.name,
                used_mb,
                limit_mb,
                "instance over disk quota — will be stopped next sweep if still over"
            );
            db::audit(
                &state.db,
                "system",
                "disk_quota_exceeded",
                &instance.name,
                &format!("{used_mb} MB used, limit {limit_mb} MB"),
            )
            .await;
            notify::deliver(
                state,
                instance,
                &notify::Push {
                    title: "Disk quota exceeded".into(),
                    body: format!(
                        "{} is using {used_mb} MB of its {limit_mb} MB disk allowance. \
                         Clean up old files now — it will be stopped soon if it stays \
                         over the limit.",
                        instance.name
                    ),
                    priority: "high",
                    dedup_id: None,
                },
            )
            .await;
        }
        QuotaAction::Stop => {
            tracing::warn!(
                instance = %instance.name,
                used_mb,
                limit_mb,
                "stopping instance: still over disk quota after warning"
            );
            if let Err(e) = state.docker.stop(&instance.container_name).await {
                tracing::error!(
                    instance = %instance.name,
                    error = format!("{e:#}"),
                    "disk-quota stop failed"
                );
                // Stay in Warned so the next sweep retries the stop.
                flags.insert(instance.id.clone(), QuotaState::Warned);
                return;
            }
            db::audit(
                &state.db,
                "system",
                "disk_quota_stopped",
                &instance.name,
                &format!("{used_mb} MB used, limit {limit_mb} MB"),
            )
            .await;
            notify::deliver(
                state,
                instance,
                &notify::Push {
                    title: "Stopped: disk quota".into(),
                    body: format!(
                        "{} was stopped because it is still using {used_mb} MB of its \
                         {limit_mb} MB disk allowance. Starting it grants a few minutes \
                         to clean up; otherwise ask the operator to raise the limit.",
                        instance.name
                    ),
                    priority: "high",
                    dedup_id: None,
                },
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use QuotaAction as A;
    use QuotaState as S;

    #[test]
    fn escalates_warn_then_stop() {
        let (s, a) = quota_step(None, true, false, true, true);
        assert_eq!((s, a), (Some(S::Warned), A::Warn));
        let (s, a) = quota_step(s, true, false, true, true);
        assert_eq!((s, a), (Some(S::Stopped), A::Stop));
        // Stopped + still over + not running: nothing more to do.
        let (s, a) = quota_step(s, true, false, false, true);
        assert_eq!((s, a), (Some(S::Stopped), A::None));
    }

    #[test]
    fn warn_only_mode_never_stops() {
        let (s, a) = quota_step(None, true, false, true, false);
        assert_eq!((s, a), (Some(S::Warned), A::Warn));
        let (s, a) = quota_step(s, true, false, true, false);
        assert_eq!((s, a), (Some(S::Warned), A::None));
    }

    #[test]
    fn restart_gets_one_sweep_of_cleanup_grace() {
        // Stopped instance started back up while still over: demoted to
        // Warned with no action this sweep, stopped again on the next.
        let (s, a) = quota_step(Some(S::Stopped), true, false, true, true);
        assert_eq!((s, a), (Some(S::Warned), A::None));
        let (s, a) = quota_step(s, true, false, true, true);
        assert_eq!((s, a), (Some(S::Stopped), A::Stop));
    }

    #[test]
    fn stop_deferred_until_container_runs() {
        // Warned but container already stopped (e.g. by the operator):
        // no stop call; the flag holds so a restart while over gets
        // stopped on the sweep after it reappears running.
        let (s, a) = quota_step(Some(S::Warned), true, false, false, true);
        assert_eq!((s, a), (Some(S::Warned), A::None));
    }

    #[test]
    fn rearm_has_hysteresis() {
        // Dropping under the limit but above 90%: flag held, no re-warn
        // spam on the next crossing — but fully under 90% clears it.
        let (s, a) = quota_step(Some(S::Warned), false, false, true, true);
        assert_eq!((s, a), (Some(S::Warned), A::None));
        let (s, a) = quota_step(Some(S::Warned), false, true, true, true);
        assert_eq!((s, a), (None, A::None));
        // And a fresh crossing after a full re-arm warns again.
        let (s, a) = quota_step(s, true, false, true, true);
        assert_eq!((s, a), (Some(S::Warned), A::Warn));
    }
}
