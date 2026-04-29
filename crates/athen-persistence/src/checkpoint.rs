//! Atomic checkpoint save and restore with file-based crash recovery.

use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use athen_core::error::{AthenError, Result};
use athen_core::task::TaskId;
use athen_core::traits::persistence::PersistentStore;

use crate::store::SqliteStore;

/// Manages atomic checkpoint save/restore operations.
///
/// Combines database-level checkpoints (via `SqliteStore`) with optional
/// file-based atomic writes for crash recovery of critical state.
pub struct CheckpointManager {
    store: SqliteStore,
    checkpoint_dir: Option<PathBuf>,
}

/// On-disk checkpoint format including integrity verification.
#[derive(serde::Serialize, serde::Deserialize)]
struct FileCheckpoint {
    task_id: String,
    data: Value,
    checksum: String,
}

impl CheckpointManager {
    /// Create a `CheckpointManager` that only uses the database.
    pub fn new(store: SqliteStore) -> Self {
        Self {
            store,
            checkpoint_dir: None,
        }
    }

    /// Create a `CheckpointManager` with file-based atomic checkpoints.
    ///
    /// The directory will be created if it does not exist.
    pub fn with_file_backup(store: SqliteStore, dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            store,
            checkpoint_dir: Some(dir),
        })
    }

    /// Save a checkpoint for a task.
    ///
    /// Writes to the database first, then optionally performs an atomic
    /// file write (temp -> fsync -> rename) for crash recovery.
    pub async fn save(&self, task_id: TaskId, data: Value) -> Result<()> {
        // Save to database
        self.store.save_checkpoint(task_id, data.clone()).await?;

        // If file backup is configured, perform atomic file write
        if let Some(ref dir) = self.checkpoint_dir {
            let dir = dir.clone();
            tokio::task::spawn_blocking(move || atomic_file_save(&dir, task_id, &data))
                .await
                .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?
                .map_err(|e| {
                    tracing::warn!("File checkpoint save failed (DB save succeeded): {e}");
                    e
                })?;
        }

        Ok(())
    }

    /// Load a checkpoint for a task.
    ///
    /// Tries the database first. If that fails or returns None and file backup
    /// is configured, falls back to the file-based checkpoint.
    pub async fn load(&self, task_id: TaskId) -> Result<Option<Value>> {
        // Try database first
        match self.store.load_checkpoint(task_id).await {
            Ok(Some(data)) => return Ok(Some(data)),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!("DB checkpoint load failed, trying file fallback: {e}");
            }
        }

        // Fall back to file checkpoint if available
        if let Some(ref dir) = self.checkpoint_dir {
            let dir = dir.clone();
            return tokio::task::spawn_blocking(move || atomic_file_load(&dir, task_id))
                .await
                .map_err(|e| AthenError::Other(format!("Spawn blocking: {e}")))?;
        }

        Ok(None)
    }

    /// Return a reference to the underlying store.
    pub fn store(&self) -> &SqliteStore {
        &self.store
    }
}

/// Compute SHA-256 hex digest of a byte slice.
fn compute_checksum(data: &[u8]) -> String {
    format!("{:x}", Sha256::digest(data))
}

/// Checkpoint file path for a given task id.
fn checkpoint_path(dir: &Path, task_id: Uuid) -> PathBuf {
    dir.join(format!("{task_id}.checkpoint.json"))
}

/// Temporary file path during atomic write.
fn temp_path(dir: &Path, task_id: Uuid) -> PathBuf {
    dir.join(format!("{task_id}.checkpoint.tmp"))
}

/// Atomically write a checkpoint file: write to temp -> fsync -> rename.
fn atomic_file_save(dir: &Path, task_id: TaskId, data: &Value) -> Result<()> {
    use std::io::Write;

    let data_json = serde_json::to_string(data).map_err(AthenError::Serialization)?;
    let checksum = compute_checksum(data_json.as_bytes());

    let file_checkpoint = FileCheckpoint {
        task_id: task_id.to_string(),
        data: data.clone(),
        checksum,
    };

    let serialized =
        serde_json::to_string_pretty(&file_checkpoint).map_err(AthenError::Serialization)?;

    let tmp = temp_path(dir, task_id);
    let final_path = checkpoint_path(dir, task_id);

    // Write to temp file
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(serialized.as_bytes())?;
        f.sync_all()?; // fsync
    }

    // Atomic rename
    std::fs::rename(&tmp, &final_path)?;

    Ok(())
}

/// Load and verify a file-based checkpoint.
fn atomic_file_load(dir: &Path, task_id: TaskId) -> Result<Option<Value>> {
    let path = checkpoint_path(dir, task_id);

    if !path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&path)?;
    let file_checkpoint: FileCheckpoint =
        serde_json::from_str(&contents).map_err(AthenError::Serialization)?;

    // Verify integrity
    let data_json =
        serde_json::to_string(&file_checkpoint.data).map_err(AthenError::Serialization)?;
    let computed = compute_checksum(data_json.as_bytes());

    if computed != file_checkpoint.checksum {
        return Err(AthenError::Other(format!(
            "File checkpoint integrity check failed for task {task_id}: \
             expected {}, got {computed}",
            file_checkpoint.checksum
        )));
    }

    Ok(Some(file_checkpoint.data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
    use rusqlite::Connection;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    async fn setup_manager() -> CheckpointManager {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = SqliteStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init schema");
        CheckpointManager::new(store)
    }

    async fn setup_manager_with_dir(dir: &Path) -> CheckpointManager {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        let store = SqliteStore::new(Arc::new(Mutex::new(conn)));
        store.init_schema().await.expect("init schema");
        CheckpointManager::with_file_backup(store, dir).expect("with file backup")
    }

    #[tokio::test]
    async fn test_checkpoint_round_trip() {
        let manager = setup_manager().await;
        let task_id = Uuid::new_v4();
        let data = serde_json::json!({"step": 3, "state": "processing"});

        manager.save(task_id, data.clone()).await.unwrap();
        let loaded = manager.load(task_id).await.unwrap();

        assert_eq!(loaded, Some(data));
    }

    #[tokio::test]
    async fn test_checkpoint_not_found() {
        let manager = setup_manager().await;
        let loaded = manager.load(Uuid::new_v4()).await.unwrap();
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn test_checkpoint_overwrite() {
        let manager = setup_manager().await;
        let task_id = Uuid::new_v4();

        manager
            .save(task_id, serde_json::json!({"v": 1}))
            .await
            .unwrap();
        manager
            .save(task_id, serde_json::json!({"v": 2}))
            .await
            .unwrap();

        let loaded = manager.load(task_id).await.unwrap().unwrap();
        assert_eq!(loaded, serde_json::json!({"v": 2}));
    }

    #[tokio::test]
    async fn test_file_checkpoint_round_trip() {
        let tmp_dir = std::env::temp_dir().join(format!("athen_test_{}", Uuid::new_v4()));
        let manager = setup_manager_with_dir(&tmp_dir).await;
        let task_id = Uuid::new_v4();
        let data = serde_json::json!({"file_test": true});

        manager.save(task_id, data.clone()).await.unwrap();

        // Verify file was created
        let path = checkpoint_path(&tmp_dir, task_id);
        assert!(path.exists());

        // Load should work
        let loaded = manager.load(task_id).await.unwrap();
        assert_eq!(loaded, Some(data));

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[tokio::test]
    async fn test_file_checkpoint_integrity_verification() {
        let tmp_dir = std::env::temp_dir().join(format!("athen_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let task_id = Uuid::new_v4();

        // Write a corrupted checkpoint file
        let corrupted = FileCheckpoint {
            task_id: task_id.to_string(),
            data: serde_json::json!({"corrupted": true}),
            checksum: "bad_checksum".to_string(),
        };
        let path = checkpoint_path(&tmp_dir, task_id);
        std::fs::write(&path, serde_json::to_string(&corrupted).unwrap()).unwrap();

        // Loading should fail integrity check
        let result = atomic_file_load(&tmp_dir, task_id);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("integrity check failed"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
