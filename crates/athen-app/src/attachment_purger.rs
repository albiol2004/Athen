//! Background sweep that deletes attachment bytes older than the policy
//! TTL while keeping the extracted-text sidecar (small, useful for arc
//! continuity).
//!
//! The purger runs forever once spawned, waking up at `sweep_interval`
//! and walking the rows whose `fetched_at` is older than `ttl_days`.
//! For each row it best-effort deletes `local_path` from disk, then
//! flips `local_path = NULL` + stamps `purged_at` via
//! `AttachmentStore::mark_purged`. Any I/O error logs a warning and
//! moves on — a stuck file shouldn't block the rest of the sweep.

use std::time::Duration;

use chrono::{DateTime, Utc};

use athen_persistence::attachments::AttachmentStore;

/// Default cadence: hourly. The TTL itself is days, so anything
/// finer-grained burns CPU without changing user-visible behaviour.
pub const DEFAULT_SWEEP_INTERVAL: Duration = Duration::from_secs(3600);

/// One sweep pass. Returns `(considered, deleted)` so callers (and the
/// caller's tests) can assert behaviour without inspecting the DB.
///
/// Best-effort: every fs/SQLite error logs a warning and advances to
/// the next row; the only thing that aborts the sweep is `list_purgeable`
/// itself failing, which is returned to the caller.
pub async fn sweep_once(
    store: &AttachmentStore,
    cutoff: DateTime<Utc>,
) -> athen_core::error::Result<(usize, usize)> {
    let purgeable = store.list_purgeable(cutoff).await?;
    let considered = purgeable.len();
    if considered == 0 {
        return Ok((0, 0));
    }

    let mut deleted = 0usize;
    for (id, path) in purgeable {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {
                tracing::info!(
                    attachment_id = %id,
                    path = %path.display(),
                    "Deleted attachment bytes (TTL purge)"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Already gone — still mark purged so the row is
                // consistent and we stop reconsidering it.
                tracing::debug!(
                    attachment_id = %id,
                    path = %path.display(),
                    "Attachment file already missing on disk; marking purged"
                );
            }
            Err(e) => {
                tracing::warn!(
                    attachment_id = %id,
                    path = %path.display(),
                    error = %e,
                    "Failed to delete attachment file; will retry next sweep"
                );
                continue;
            }
        }
        if let Err(e) = store.mark_purged(id).await {
            tracing::warn!(
                attachment_id = %id,
                error = %e,
                "Failed to mark attachment purged after delete"
            );
            continue;
        }
        deleted += 1;
    }

    if deleted > 0 {
        tracing::info!(considered, deleted, "Attachment TTL sweep complete");
    }
    Ok((considered, deleted))
}

/// Spawn the forever-loop. The task wakes every `interval`, computes
/// `cutoff = now - ttl_days`, runs `sweep_once`, then sleeps. Owned by
/// `AppState` via the spawned join handle (not awaited — runs until
/// the process exits).
///
/// Uses `tauri::async_runtime::spawn` rather than `tokio::spawn` so the
/// purger can be started from `setup()` (synchronous Tauri callback,
/// no enclosing runtime). The Tauri runtime is itself tokio-backed, so
/// this is the same as `tokio::spawn` from a runtime-aware context.
pub fn spawn_loop(
    store: AttachmentStore,
    ttl_days: u32,
    interval: Duration,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        tracing::info!(
            ttl_days,
            interval_secs = interval.as_secs(),
            "Attachment TTL purger started"
        );
        loop {
            let cutoff = Utc::now() - chrono::Duration::days(i64::from(ttl_days));
            if let Err(e) = sweep_once(&store, cutoff).await {
                tracing::warn!(error = %e, "Attachment TTL sweep failed");
            }
            tokio::time::sleep(interval).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_core::event::Attachment;
    use athen_persistence::Database;
    use std::io::Write;

    async fn fresh_store() -> (Database, AttachmentStore) {
        let db = Database::in_memory().await.unwrap();
        let store = db.attachment_store();
        store.init_schema().await.unwrap();
        (db, store)
    }

    fn write_temp(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "athen-purger-test-{}-{}",
            uuid::Uuid::new_v4(),
            name
        ));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents).unwrap();
        p
    }

    /// A row that's still inside the TTL must not be touched.
    #[tokio::test]
    async fn sweep_skips_recent_rows() {
        let (_db, store) = fresh_store().await;
        let event_id = uuid::Uuid::new_v4();
        let path = write_temp("recent.pdf", b"%PDF-recent");
        let att = Attachment::new(
            "recent.pdf",
            "application/pdf",
            10,
            Some(path.clone()),
            None,
        );
        store.insert(event_id, &att).await.unwrap();

        // Cutoff in the past — every row's fetched_at is "after" cutoff.
        let cutoff = Utc::now() - chrono::Duration::days(30);
        let (considered, deleted) = sweep_once(&store, cutoff).await.unwrap();
        assert_eq!(considered, 0);
        assert_eq!(deleted, 0);

        // File still exists.
        assert!(path.exists());
        // Row still has local_path.
        let row = store.get(att.id).await.unwrap().unwrap();
        assert!(row.local_path.is_some());
        assert!(!row.is_purged());

        let _ = std::fs::remove_file(&path);
    }

    /// A row past the TTL gets its file deleted, local_path nulled,
    /// purged_at stamped. The extracted_text sidecar survives.
    #[tokio::test]
    async fn sweep_purges_old_rows_and_keeps_sidecar() {
        let (_db, store) = fresh_store().await;
        let event_id = uuid::Uuid::new_v4();
        let path = write_temp("old.pdf", b"%PDF-old");
        let sidecar = write_temp("old.pdf.txt", b"extracted text");
        let mut att = Attachment::new("old.pdf", "application/pdf", 10, Some(path.clone()), None);
        att.extracted_text_path = Some(sidecar.clone());
        store.insert(event_id, &att).await.unwrap();

        // Cutoff in the future — every row counts as "old".
        let cutoff = Utc::now() + chrono::Duration::days(1);
        let (considered, deleted) = sweep_once(&store, cutoff).await.unwrap();
        assert_eq!(considered, 1);
        assert_eq!(deleted, 1);

        assert!(!path.exists(), "Bytes should be deleted from disk");
        assert!(sidecar.exists(), "Sidecar must survive the purge");

        let row = store.get(att.id).await.unwrap().unwrap();
        assert!(row.local_path.is_none());
        assert!(row.is_purged());
        assert_eq!(row.extracted_text_path, Some(sidecar.clone()));

        let _ = std::fs::remove_file(&sidecar);
    }

    /// File already missing: still mark the row purged so we don't
    /// re-consider it forever.
    #[tokio::test]
    async fn sweep_handles_missing_file() {
        let (_db, store) = fresh_store().await;
        let event_id = uuid::Uuid::new_v4();
        let bogus =
            std::env::temp_dir().join(format!("athen-missing-{}.pdf", uuid::Uuid::new_v4()));
        let att = Attachment::new(
            "missing.pdf",
            "application/pdf",
            10,
            Some(bogus.clone()),
            None,
        );
        store.insert(event_id, &att).await.unwrap();

        let cutoff = Utc::now() + chrono::Duration::days(1);
        let (considered, deleted) = sweep_once(&store, cutoff).await.unwrap();
        assert_eq!(considered, 1);
        assert_eq!(deleted, 1);

        let row = store.get(att.id).await.unwrap().unwrap();
        assert!(row.is_purged());
        assert!(row.local_path.is_none());
    }

    /// Already-purged rows must not be reconsidered.
    #[tokio::test]
    async fn sweep_does_not_reconsider_purged_rows() {
        let (_db, store) = fresh_store().await;
        let event_id = uuid::Uuid::new_v4();
        let path = write_temp("once.pdf", b"%PDF-once");
        let att = Attachment::new("once.pdf", "application/pdf", 10, Some(path.clone()), None);
        store.insert(event_id, &att).await.unwrap();

        let cutoff = Utc::now() + chrono::Duration::days(1);
        let (_, deleted_first) = sweep_once(&store, cutoff).await.unwrap();
        assert_eq!(deleted_first, 1);

        // Second pass at the same cutoff: nothing left to do.
        let (considered, deleted_second) = sweep_once(&store, cutoff).await.unwrap();
        assert_eq!(considered, 0);
        assert_eq!(deleted_second, 0);
    }
}
