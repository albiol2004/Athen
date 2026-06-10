//! Tauri commands driving the bundled fastembed-rs embedder UI.
//!
//! Frontend uses these to (a) ask "what tier do you recommend for this
//! box?", (b) check which tiers already have weights cached, (c) trigger
//! a (long) download for a tier, (d) delete a cached tier, and
//! (e) switch the active embedding mode to `Bundled { tier }`.
//!
//! Downloads are gated on the `bundled-embeddings` cargo feature. When
//! the feature is OFF, the download command exists but errors out
//! cleanly so the frontend can render a sensible message instead of
//! seeing the command vanish from the invoke handler.
//!
//! Cache layout follows the HuggingFace hub convention used by
//! fastembed-rs:
//!
//! ```text
//! <cache_dir>/models--<org>--<repo>/snapshots/<sha>/<files…>
//! ```
//!
//! Tier → repo mapping:
//!
//! | Tier         | HF repo                                |
//! |--------------|----------------------------------------|
//! | Light        | `intfloat/multilingual-e5-small`       |
//! | Standard     | `intfloat/multilingual-e5-base`        |
//! | HighQuality  | `BAAI/bge-m3`                          |
//!
//! Downloaded detection: the snapshot dir for the tier exists and
//! contains at least one `.onnx` file.

use std::path::{Path, PathBuf};

use athen_core::config::{BundledTier, EmbeddingMode};
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

use crate::embedding_hardware::{system_summary, SystemSummary};
use crate::state::AppState;

/// Snapshot of bundled-embedding state surfaced to the frontend.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BundledEmbeddingStatus {
    /// Tiers whose weights are already cached locally.
    pub downloaded_tiers: Vec<BundledTier>,
    /// Current `EmbeddingMode::Bundled` tier, if any.
    pub active_tier: Option<BundledTier>,
    /// Absolute path to the on-disk cache directory.
    pub cache_dir: String,
    /// Sum of cached weight bytes across all tiers, in MiB.
    pub total_cache_size_mb: u64,
}

/// Payload of the `embedding-download-progress` event.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
struct DownloadProgress {
    tier: BundledTier,
    /// `starting` | `downloading` | `complete` | `failed`. We don't
    /// emit a separate `loading` phase — fastembed v5.14 fuses fetch
    /// and ONNX load inside `TextEmbedding::try_new`, so the frontend
    /// shows an indeterminate spinner across the whole `downloading`
    /// phase.
    phase: &'static str,
    message: String,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Inspect the host and return the SystemSummary (incl. recommended tier).
#[tauri::command]
pub async fn recommend_embedding_tier() -> std::result::Result<SystemSummary, String> {
    let data_dir = athen_core::paths::athen_data_dir()
        .unwrap_or_else(|| std::env::temp_dir().join("athen-recommend-fallback"));
    Ok(system_summary(&data_dir))
}

/// Report which tiers are cached locally + which is active + total bytes.
#[tauri::command]
pub async fn get_bundled_embedding_status(
    _state: State<'_, AppState>,
) -> std::result::Result<BundledEmbeddingStatus, String> {
    get_bundled_embedding_status_core(&_state).await
}

pub(crate) async fn get_bundled_embedding_status_core(
    _state: &AppState,
) -> std::result::Result<BundledEmbeddingStatus, String> {
    let cache_dir =
        bundled_cache_dir().ok_or_else(|| "no Athen data dir resolvable".to_string())?;

    let downloaded_tiers = scan_downloaded_tiers(&cache_dir);
    let total_cache_size_mb: u64 = downloaded_tiers
        .iter()
        .map(|t| tier_cache_bytes(&cache_dir, *t) / (1024 * 1024))
        .sum();

    let config = crate::settings::load_main_config_public();
    let active_tier = match config.embeddings.mode {
        EmbeddingMode::Bundled { tier } => Some(tier),
        _ => None,
    };

    Ok(BundledEmbeddingStatus {
        downloaded_tiers,
        active_tier,
        cache_dir: cache_dir.to_string_lossy().to_string(),
        total_cache_size_mb,
    })
}

/// Download (or re-download) the weights for `tier`. Long-running.
///
/// Emits `embedding-download-progress` events as it transitions through
/// the `starting → downloading → (complete|failed)` lifecycle. Returns
/// only once the model is fully fetched + loaded into memory, or once an
/// error is captured. The `loading` phase isn't emitted separately —
/// fastembed v5.14 has no callback boundary between fetch and load.
#[tauri::command]
pub async fn download_bundled_model(
    tier: BundledTier,
    app: AppHandle,
    _state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    emit_progress(
        &app,
        tier,
        "starting",
        format!("Preparing {} download…", tier_label(tier)),
    );

    #[cfg(feature = "bundled-embeddings")]
    {
        let cache_dir = match bundled_cache_dir() {
            Some(d) => d,
            None => {
                let msg = "no Athen data dir resolvable".to_string();
                emit_progress(&app, tier, "failed", msg.clone());
                return Err(msg);
            }
        };
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            let msg = format!("create cache dir {}: {}", cache_dir.display(), e);
            emit_progress(&app, tier, "failed", msg.clone());
            return Err(msg);
        }

        emit_progress(
            &app,
            tier,
            "downloading",
            format!(
                "Downloading {} (~{} MB)…",
                tier_label(tier),
                tier.approx_disk_mb()
            ),
        );

        let provider =
            athen_llm::embeddings::bundled::BundledEmbedding::new(cache_dir.clone(), tier);
        // fastembed exposes init only through embed() — a single short
        // warmup string triggers download + ONNX load and returns once
        // both are done. The vector itself is discarded.
        let result = {
            use athen_core::traits::embedding::EmbeddingProvider;
            provider.embed("warmup").await
        };

        match result {
            Ok(_) => {
                emit_progress(
                    &app,
                    tier,
                    "complete",
                    format!("{} ready.", tier_label(tier)),
                );
                Ok(())
            }
            Err(e) => {
                let msg = format!("download/load failed: {}", e);
                emit_progress(&app, tier, "failed", msg.clone());
                Err(msg)
            }
        }
    }

    #[cfg(not(feature = "bundled-embeddings"))]
    {
        let msg = "bundled embeddings not compiled into this build".to_string();
        emit_progress(&app, tier, "failed", msg.clone());
        Err(msg)
    }
}

/// Delete the cached files for `tier`. Idempotent — succeeds if nothing
/// is cached.
#[tauri::command]
pub async fn delete_bundled_model(
    tier: BundledTier,
    _state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    let cache_dir =
        bundled_cache_dir().ok_or_else(|| "no Athen data dir resolvable".to_string())?;
    let repo_dir = cache_dir.join(repo_dir_name(tier));
    if !repo_dir.exists() {
        return Ok(());
    }
    tokio::fs::remove_dir_all(&repo_dir)
        .await
        .map_err(|e| format!("remove {}: {}", repo_dir.display(), e))?;
    // fastembed/hf-hub may also leave .lock files under cache_dir
    // (e.g. `<cache_dir>/models--<org>--<repo>.lock`). Sweep them too.
    let lock_name = format!("{}.lock", repo_dir_name(tier));
    let lock_path = cache_dir.join(&lock_name);
    if lock_path.exists() {
        let _ = tokio::fs::remove_file(&lock_path).await;
    }
    Ok(())
}

/// Switch `EmbeddingMode` to `Bundled { tier }` and persist. The
/// embedding router currently rebuilds only at app boot — we surface
/// that in the success message so the frontend can prompt for a
/// restart.
#[tauri::command]
pub async fn set_embedding_mode_bundled(
    tier: BundledTier,
    state: State<'_, AppState>,
) -> std::result::Result<(), String> {
    set_embedding_mode_bundled_core(tier, &state).await
}

pub(crate) async fn set_embedding_mode_bundled_core(
    tier: BundledTier,
    state: &AppState,
) -> std::result::Result<(), String> {
    let mode_str = match tier {
        BundledTier::Light => "Bundled:light",
        BundledTier::Standard => "Bundled:standard",
        BundledTier::HighQuality => "Bundled:high-quality",
    };
    // TODO: when AppState gains a `rebuild_embedding_router` hook,
    // call it here so the change applies without a restart.
    crate::settings::save_embedding_settings_core(
        state,
        mode_str.to_string(),
        None,
        None,
        None,
        None,
    )
    .await
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn emit_progress(app: &AppHandle, tier: BundledTier, phase: &'static str, message: String) {
    let payload = DownloadProgress {
        tier,
        phase,
        message,
    };
    if let Err(e) = app.emit("embedding-download-progress", payload) {
        tracing::warn!(error = %e, "failed to emit embedding-download-progress");
    }
}

fn bundled_cache_dir() -> Option<PathBuf> {
    athen_core::paths::athen_data_dir().map(|d| d.join("embeddings"))
}

/// HF-hub repo directory name for a tier, e.g.
/// `models--intfloat--multilingual-e5-small`.
pub(crate) fn repo_dir_name(tier: BundledTier) -> String {
    let (org, repo) = repo_for(tier);
    format!("models--{}--{}", org, repo)
}

/// Tier → `(org, repo)` for the cached HF model.
pub(crate) fn repo_for(tier: BundledTier) -> (&'static str, &'static str) {
    match tier {
        BundledTier::Light => ("intfloat", "multilingual-e5-small"),
        BundledTier::Standard => ("intfloat", "multilingual-e5-base"),
        BundledTier::HighQuality => ("BAAI", "bge-m3"),
    }
}

fn tier_label(tier: BundledTier) -> &'static str {
    match tier {
        BundledTier::Light => "Light",
        BundledTier::Standard => "Standard",
        BundledTier::HighQuality => "High quality",
    }
}

/// True iff `cache_dir/<repo_dir>/snapshots/<sha>/` exists and contains
/// at least one `.onnx` file in any subdirectory of any snapshot.
pub(crate) fn is_tier_downloaded(cache_dir: &Path, tier: BundledTier) -> bool {
    let snapshots = cache_dir.join(repo_dir_name(tier)).join("snapshots");
    let Ok(read) = std::fs::read_dir(&snapshots) else {
        return false;
    };
    for snapshot in read.flatten() {
        let path = snapshot.path();
        if !path.is_dir() {
            continue;
        }
        if dir_has_onnx(&path) {
            return true;
        }
    }
    false
}

/// Recursively check `dir` for any `.onnx` file. fastembed/hf-hub keeps
/// the actual weights inside `<snapshot>/onnx/` for some repos and at
/// the snapshot root for others — walking handles both shapes.
fn dir_has_onnx(dir: &Path) -> bool {
    let Ok(read) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if dir_has_onnx(&path) {
                return true;
            }
            continue;
        }
        if path.extension().is_some_and(|e| e == "onnx") {
            return true;
        }
    }
    false
}

pub(crate) fn scan_downloaded_tiers(cache_dir: &Path) -> Vec<BundledTier> {
    let mut out = Vec::new();
    for tier in [
        BundledTier::Light,
        BundledTier::Standard,
        BundledTier::HighQuality,
    ] {
        if is_tier_downloaded(cache_dir, tier) {
            out.push(tier);
        }
    }
    out
}

/// Total bytes consumed by all files under `cache_dir/<repo_dir>/`.
/// Walks symlinks via `read_dir` (no follow) to stay on the configured
/// cache mount.
pub(crate) fn tier_cache_bytes(cache_dir: &Path, tier: BundledTier) -> u64 {
    let root = cache_dir.join(repo_dir_name(tier));
    if !root.exists() {
        return 0;
    }
    walk_size(&root)
}

fn walk_size(dir: &Path) -> u64 {
    let Ok(read) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in read.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {
                total = total.saturating_add(walk_size(&path));
            }
            Ok(ft) if ft.is_file() => {
                if let Ok(meta) = entry.metadata() {
                    total = total.saturating_add(meta.len());
                }
            }
            _ => {}
        }
    }
    total
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(p, b"x").expect("write");
    }

    #[test]
    fn repo_mapping_matches_fastembed_choices() {
        assert_eq!(
            repo_for(BundledTier::Light),
            ("intfloat", "multilingual-e5-small")
        );
        assert_eq!(
            repo_for(BundledTier::Standard),
            ("intfloat", "multilingual-e5-base")
        );
        assert_eq!(repo_for(BundledTier::HighQuality), ("BAAI", "bge-m3"));
        assert_eq!(
            repo_dir_name(BundledTier::Light),
            "models--intfloat--multilingual-e5-small"
        );
        assert_eq!(
            repo_dir_name(BundledTier::HighQuality),
            "models--BAAI--bge-m3"
        );
    }

    #[test]
    fn empty_cache_dir_reports_nothing_downloaded() {
        let tmp = tempdir().expect("tempdir");
        assert!(!is_tier_downloaded(tmp.path(), BundledTier::Light));
        assert!(scan_downloaded_tiers(tmp.path()).is_empty());
    }

    #[test]
    fn snapshot_with_onnx_at_root_is_detected() {
        let tmp = tempdir().expect("tempdir");
        let onnx = tmp
            .path()
            .join(repo_dir_name(BundledTier::Light))
            .join("snapshots")
            .join("deadbeef")
            .join("model.onnx");
        touch(&onnx);
        assert!(is_tier_downloaded(tmp.path(), BundledTier::Light));
        assert!(!is_tier_downloaded(tmp.path(), BundledTier::Standard));
    }

    #[test]
    fn snapshot_with_onnx_nested_under_subdir_is_detected() {
        // BGE-M3 in particular ships its weights under `<snapshot>/onnx/`.
        let tmp = tempdir().expect("tempdir");
        let onnx = tmp
            .path()
            .join(repo_dir_name(BundledTier::HighQuality))
            .join("snapshots")
            .join("cafe1234")
            .join("onnx")
            .join("model.onnx");
        touch(&onnx);
        assert!(is_tier_downloaded(tmp.path(), BundledTier::HighQuality));
    }

    #[test]
    fn snapshot_without_onnx_is_not_downloaded() {
        // Partial/aborted download — config.json present, no .onnx.
        let tmp = tempdir().expect("tempdir");
        let cfg = tmp
            .path()
            .join(repo_dir_name(BundledTier::Standard))
            .join("snapshots")
            .join("0badf00d")
            .join("config.json");
        touch(&cfg);
        assert!(!is_tier_downloaded(tmp.path(), BundledTier::Standard));
    }

    #[test]
    fn scan_finds_multiple_downloaded_tiers() {
        let tmp = tempdir().expect("tempdir");
        touch(
            &tmp.path()
                .join(repo_dir_name(BundledTier::Light))
                .join("snapshots/aaa/model.onnx"),
        );
        touch(
            &tmp.path()
                .join(repo_dir_name(BundledTier::Standard))
                .join("snapshots/bbb/onnx/model.onnx"),
        );
        let tiers = scan_downloaded_tiers(tmp.path());
        assert_eq!(tiers.len(), 2);
        assert!(tiers.contains(&BundledTier::Light));
        assert!(tiers.contains(&BundledTier::Standard));
        assert!(!tiers.contains(&BundledTier::HighQuality));
    }

    #[test]
    fn tier_cache_bytes_sums_all_files() {
        let tmp = tempdir().expect("tempdir");
        let base = tmp
            .path()
            .join(repo_dir_name(BundledTier::Light))
            .join("snapshots/aaa");
        fs::create_dir_all(&base).expect("mkdir");
        fs::write(base.join("model.onnx"), vec![0u8; 1024]).expect("w1");
        fs::write(base.join("tokenizer.json"), vec![0u8; 256]).expect("w2");
        let bytes = tier_cache_bytes(tmp.path(), BundledTier::Light);
        assert_eq!(bytes, 1024 + 256);
        // Unset tiers count as zero.
        assert_eq!(tier_cache_bytes(tmp.path(), BundledTier::HighQuality), 0);
    }
}
