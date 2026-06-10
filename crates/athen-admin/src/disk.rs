//! Disk usage sweep + soft quotas.
//!
//! Docker's hard disk quota (`storage_opt: size=`) only works when the
//! storage driver sits on xfs with project quotas — on every other setup
//! (including stock Fedora/Ubuntu hosts) container creation fails
//! outright. So disk quotas here are SOFT: a periodic `docker system df`
//! sweep measures each instance's data-volume usage, the dashboard shows
//! it, and crossing the per-instance `disk_limit_mb` raises an audit row
//! plus a webhook push to the granted users. The warning fires once per
//! crossing and re-arms only after usage falls back under 90% of the
//! limit, so a volume hovering at the threshold doesn't spam phones.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::db::{self, Instance};
use crate::{instances, notify, PanelState};

/// `docker system df` walks every volume — cheap at panel scale (tens of
/// volumes), but not something to hammer. Usage display tolerates 5 min
/// of staleness.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// Warning re-arms only after usage falls below this fraction of the
/// limit (hysteresis against threshold-hovering volumes).
const REARM_FRACTION: f64 = 0.9;

/// Spawn the disk sweep loop. Returns immediately.
pub fn spawn(state: Arc<PanelState>) {
    tokio::spawn(run(state));
}

async fn run(state: Arc<PanelState>) {
    // Instances currently flagged over-quota (warning already sent).
    let mut over: HashSet<String> = HashSet::new();
    loop {
        if let Err(e) = sweep(&state, &mut over).await {
            tracing::debug!(error = format!("{e:#}"), "disk sweep failed (docker down?)");
        }
        tokio::time::sleep(SWEEP_INTERVAL).await;
    }
}

async fn sweep(state: &Arc<PanelState>, over: &mut HashSet<String>) -> anyhow::Result<()> {
    let usage = state.docker.volume_usage_bytes().await?;
    let all = instances::list_all(&state.db).await?;
    let mut snapshot: HashMap<String, u64> = HashMap::new();
    for instance in all {
        let Some(bytes) = usage.get(&instance.volume_name).copied() else {
            continue; // volume gone or size unknown (non-local driver)
        };
        snapshot.insert(instance.id.clone(), bytes);
        check_quota(state, &instance, bytes, over).await;
    }
    *state
        .disk_usage
        .lock()
        .expect("disk usage mutex poisoned") = snapshot;
    Ok(())
}

async fn check_quota(
    state: &Arc<PanelState>,
    instance: &Instance,
    used_bytes: u64,
    over: &mut HashSet<String>,
) {
    let Some(limit_mb) = instance.disk_limit_mb else {
        return;
    };
    let limit_bytes = limit_mb.saturating_mul(1024 * 1024);
    let used_mb = used_bytes / (1024 * 1024);
    if used_bytes > limit_bytes {
        if over.insert(instance.id.clone()) {
            tracing::warn!(
                instance = %instance.name,
                used_mb,
                limit_mb,
                "instance over soft disk quota"
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
                         Clean up old files or ask the operator to raise the limit.",
                        instance.name
                    ),
                    priority: "high",
                    dedup_id: None,
                },
            )
            .await;
        }
    } else if (used_bytes as f64) < limit_bytes as f64 * REARM_FRACTION {
        over.remove(&instance.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rearm_math_has_hysteresis() {
        // 100 MB limit: over at >100 MB, re-arms only under 90 MB.
        let limit_bytes = 100u64 * 1024 * 1024;
        let over = |b: u64| b > limit_bytes;
        let rearmed = |b: u64| (b as f64) < limit_bytes as f64 * REARM_FRACTION;
        assert!(over(101 * 1024 * 1024));
        assert!(!over(100 * 1024 * 1024));
        assert!(!rearmed(95 * 1024 * 1024), "between 90% and 100%: stays flagged");
        assert!(rearmed(89 * 1024 * 1024));
    }
}
