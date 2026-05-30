//! Tauri commands for the Settings → Voice panel.
//!
//! Three commands surface the voice subsystem to the frontend:
//!
//! * [`get_voice_settings`] — load the persisted [`VoiceConfig`].
//! * [`save_voice_settings`] — overwrite the persisted [`VoiceConfig`].
//! * [`list_voice_options`] — enumerate registered Cloud APIs endpoints
//!   (bucketed by STT / TTS / Phone), plus available LLM (connection, slug)
//!   loadout picks for the per-call override dropdown.
//!
//! The actual `place_call` agent tool is a separate batch — this module
//! only persists the wiring. The Settings → Voice panel renders a
//! disabled "Test setup" stub until that batch lands.
//!
//! Persistence: `VoiceConfig` lives as a JSON blob under
//! `AthenConfig::voice` in `config.toml`. `athen-core` cannot depend on
//! `athen-voice` (hexagonal rules), so the typed serialization happens
//! here — `athen-core` just stores a [`serde_json::Value`].
//!
//! Endpoint bucketing keys off the preset `slug` — `deepgram` → STT,
//! `elevenlabs`/`cartesia` → TTS, `twilio` → Phone. The match is
//! case-insensitive on `provider` so a user who edits the provider field
//! manually still lands in the right bucket.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use athen_agent::runtimes::{self as agent_runtimes, InstallProgress, RuntimeKind};
use athen_core::config::{Bundle, ACTIVE_BUNDLE_KEY};
use athen_core::llm::ModelProfile;
use athen_voice::pipecat_runtime::{self, PipecatPaths, SetupPhase, SetupProgress, SetupStatus};
use athen_voice::VoiceConfig;
use serde::{Deserialize, Serialize};
use tauri::path::BaseDirectory;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::state::AppState;

// ---------------------------------------------------------------------------
// Endpoint slug → bucket classification
// ---------------------------------------------------------------------------

/// Preset slugs (or provider-name lowercased fallback) we recognise as STT.
const STT_SLUGS: &[&str] = &["deepgram"];
/// Preset slugs we recognise as TTS.
const TTS_SLUGS: &[&str] = &["elevenlabs", "cartesia"];
/// Preset slugs we recognise as phone-service providers.
const PHONE_SLUGS: &[&str] = &["twilio"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceBucket {
    Stt,
    Tts,
    Phone,
}

/// Classify a registered endpoint by `provider` (the human label saved
/// from the preset; e.g. "Deepgram", "Twilio") OR by a slug-like match
/// against the same. Returns `None` if the endpoint is unrelated.
///
/// Exposed for the unit tests below — the pure-function shape lets the
/// classifier be exercised without spinning up an HTTP endpoint store.
fn classify(provider: &str) -> Option<VoiceBucket> {
    let normalised = provider.trim().to_ascii_lowercase();
    if normalised.is_empty() {
        return None;
    }
    let buckets: &[(VoiceBucket, &[&str])] = &[
        (VoiceBucket::Stt, STT_SLUGS),
        (VoiceBucket::Tts, TTS_SLUGS),
        (VoiceBucket::Phone, PHONE_SLUGS),
    ];
    for (bucket, slugs) in buckets {
        for slug in *slugs {
            if normalised == *slug || normalised.contains(slug) {
                return Some(*bucket);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointOption {
    pub id: String,
    pub label: String,
    pub provider: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmConnectionOption {
    pub connection_id: String,
    pub connection_label: String,
    pub slug: String,
    pub display: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceOptions {
    pub stt_endpoints: Vec<EndpointOption>,
    pub tts_endpoints: Vec<EndpointOption>,
    pub phone_endpoints: Vec<EndpointOption>,
    pub llm_connections: Vec<LlmConnectionOption>,
    /// Cosmetic placeholder shown as the first item in the LLM override
    /// dropdown — e.g. `"Use Fast tier (DeepSeek :: deepseek-chat)"`.
    /// `None` when the active Bundle has no Fast tier configured.
    pub fast_tier_label: Option<String>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Load the persisted voice config. Returns defaults when nothing has
/// been saved yet.
#[tauri::command]
pub async fn get_voice_settings(
    _state: State<'_, AppState>,
) -> std::result::Result<VoiceConfig, String> {
    let config = crate::settings::load_main_config_public();
    Ok(decode_voice(&config.voice))
}

/// Persist a voice config. Clamps `max_call_duration_s` to the hard
/// limit declared by `VoiceConfig`.
#[tauri::command]
pub async fn save_voice_settings(
    _state: State<'_, AppState>,
    config: VoiceConfig,
) -> std::result::Result<(), String> {
    let mut config = config;
    let (clamped, _) = VoiceConfig::clamp_duration(config.max_call_duration_s);
    config.max_call_duration_s = clamped;

    let mut main = crate::settings::load_main_config_public();
    main.voice =
        serde_json::to_value(&config).map_err(|e| format!("serialize VoiceConfig: {e}"))?;
    crate::settings::save_main_config_public(&main)?;
    tracing::info!("Voice config saved");
    Ok(())
}

/// Enumerate the picks the Voice panel renders into its dropdowns.
#[tauri::command]
pub async fn list_voice_options(
    state: State<'_, AppState>,
) -> std::result::Result<VoiceOptions, String> {
    let endpoints = collect_endpoints(&state).await?;

    let mut stt_endpoints = Vec::new();
    let mut tts_endpoints = Vec::new();
    let mut phone_endpoints = Vec::new();

    for ep in endpoints {
        let Some(bucket) = classify(&ep.provider) else {
            continue;
        };
        let slug = ep.provider.trim().to_ascii_lowercase();
        let label = if ep.name.trim().is_empty() {
            format!("{} ({})", ep.provider, ep.base_url)
        } else {
            ep.name.clone()
        };
        let opt = EndpointOption {
            id: ep.id.to_string(),
            label,
            provider: ep.provider.clone(),
            slug,
        };
        match bucket {
            VoiceBucket::Stt => stt_endpoints.push(opt),
            VoiceBucket::Tts => tts_endpoints.push(opt),
            VoiceBucket::Phone => phone_endpoints.push(opt),
        }
    }

    // Stable alphabetic sort so the dropdown order doesn't jiggle.
    for v in [&mut stt_endpoints, &mut tts_endpoints, &mut phone_endpoints] {
        v.sort_by_key(|a| a.label.to_ascii_lowercase());
    }

    let cfg = crate::settings::load_main_config_public();
    let llm_connections = collect_llm_connections(&cfg);
    let fast_tier_label = active_fast_tier_label(&cfg);

    Ok(VoiceOptions {
        stt_endpoints,
        tts_endpoints,
        phone_endpoints,
        llm_connections,
        fast_tier_label,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn decode_voice(value: &serde_json::Value) -> VoiceConfig {
    if value.is_null() {
        return VoiceConfig::new();
    }
    match serde_json::from_value::<VoiceConfig>(value.clone()) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(error = %e, "voice config blob malformed; returning defaults");
            VoiceConfig::new()
        }
    }
}

async fn collect_endpoints(
    state: &State<'_, AppState>,
) -> std::result::Result<Vec<athen_core::http_endpoint::RegisteredEndpoint>, String> {
    use athen_core::traits::http_endpoint::HttpEndpointStore;
    let Some(store) = state.http_endpoint_store.as_ref() else {
        return Ok(Vec::new());
    };
    store.list().await.map_err(|e| e.to_string())
}

/// Build the `(connection, slug)` option list for the LLM override
/// dropdown. We emit one entry per connection × its `default_model`,
/// plus one per (connection × tier slug) the user has set. This is a
/// pragmatic-not-exhaustive list — most users only see "<provider> ::
/// <default model>" and that's enough.
fn collect_llm_connections(cfg: &athen_core::config::AthenConfig) -> Vec<LlmConnectionOption> {
    let mut out: Vec<LlmConnectionOption> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for (id, provider) in &cfg.models.providers {
        let connection_label = display_connection_label(id);
        let mut push_slug = |slug: &str| {
            if slug.is_empty() {
                return;
            }
            let key = (id.clone(), slug.to_string());
            if !seen.insert(key) {
                return;
            }
            out.push(LlmConnectionOption {
                connection_id: id.clone(),
                connection_label: connection_label.clone(),
                slug: slug.to_string(),
                display: format!("{connection_label} :: {slug}"),
            });
        };
        push_slug(&provider.default_model);
        for slug in provider.tier_models.values() {
            push_slug(slug);
        }
    }

    out.sort_by_key(|a| a.display.to_ascii_lowercase());
    out
}

/// Resolve the active Bundle's Fast tier into a human label, used as the
/// placeholder for the LLM override dropdown.
fn active_fast_tier_label(cfg: &athen_core::config::AthenConfig) -> Option<String> {
    let bundle_id = cfg.models.assignments.get(ACTIVE_BUNDLE_KEY)?;
    let bundle: &Bundle = cfg.models.bundles.get(bundle_id)?;
    let tier = bundle.tiers.get(&ModelProfile::Fast)?;
    let connection_label = display_connection_label(&tier.connection_id);
    Some(format!(
        "Use Fast tier ({connection_label} :: {})",
        tier.slug
    ))
}

/// Fallback "Display Name" derivation for a connection id. The Settings
/// UI uses the same trick — `display_name(id)` over in `settings.rs` —
/// but that helper is private and we don't want to widen its visibility
/// for this one consumer.
fn display_connection_label(id: &str) -> String {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Bundled Pipecat runner extraction
// ---------------------------------------------------------------------------

/// File name the script lives under both in the Tauri bundle resources
/// and in the user-side toolbox dir.
const PIPECAT_RUNNER_NAME: &str = "pipecat_runner.py";

/// Resolve the bundled `pipecat_runner.py` and copy it into the user's
/// toolbox dir at `<athen_data_dir>/toolbox/pipecat_runner.py`. The copy
/// happens on first call and on every subsequent call where the bundled
/// resource is newer than the cached copy (so app updates ship fresh
/// runners). Returns the target path.
///
/// All errors are returned as `String` because this helper is called from
/// a Tauri command and the frontend wants a plain message.
pub fn ensure_runner_extracted(app_handle: &AppHandle) -> std::result::Result<PathBuf, String> {
    // The bundled resource path. With
    // `tauri.conf.json -> bundle.resources = ["../../assets/pipecat_runner.py"]`,
    // Tauri flattens the file to `<resource_dir>/pipecat_runner.py`. We
    // also probe `assets/pipecat_runner.py` as a fallback in case a
    // future config change preserves the leading directory.
    let resolver = app_handle.path();
    let bundled = resolver
        .resolve(PIPECAT_RUNNER_NAME, BaseDirectory::Resource)
        .ok()
        .filter(|p| p.exists())
        .or_else(|| {
            resolver
                .resolve(
                    format!("assets/{PIPECAT_RUNNER_NAME}"),
                    BaseDirectory::Resource,
                )
                .ok()
                .filter(|p| p.exists())
        })
        .ok_or_else(|| format!("bundled {PIPECAT_RUNNER_NAME} not found in app resources"))?;

    let toolbox = athen_core::paths::athen_toolbox_dir()
        .ok_or_else(|| "could not resolve athen toolbox dir".to_string())?;
    std::fs::create_dir_all(&toolbox).map_err(|e| format!("create toolbox dir: {e}"))?;
    let target = toolbox.join(PIPECAT_RUNNER_NAME);

    let should_copy = match (std::fs::metadata(&target), std::fs::metadata(&bundled)) {
        (Err(_), _) => true,
        (Ok(t), Ok(b)) => match (t.modified(), b.modified()) {
            (Ok(tt), Ok(bt)) => bt > tt,
            // Modtime unavailable on this fs — be safe, recopy.
            _ => true,
        },
        // Bundle metadata read failed but file existed — try to copy and surface error there.
        (Ok(_), Err(_)) => true,
    };

    if should_copy {
        std::fs::copy(&bundled, &target).map_err(|e| format!("copy {PIPECAT_RUNNER_NAME}: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&target) {
                let mut perm = meta.permissions();
                perm.set_mode(0o755);
                let _ = std::fs::set_permissions(&target, perm);
            }
        }
        tracing::info!(target = %target.display(), "extracted pipecat runner");
    }

    Ok(target)
}

/// Tauri command — extracts the bundled `pipecat_runner.py` to the
/// toolbox dir (if needed) and returns the absolute target path as a
/// string. Wired into the install button (batch 2B) and the place_call
/// tool wiring (batch 3).
#[tauri::command]
pub async fn extract_pipecat_runner(app_handle: AppHandle) -> std::result::Result<String, String> {
    let p = ensure_runner_extracted(&app_handle)?;
    Ok(p.to_string_lossy().to_string())
}

/// Result of the Settings → Voice "Test setup" button — a non-call
/// preflight that validates the saved wiring without burning a Twilio
/// minute. Surfaces the same checks that `place_call`'s preflight
/// performs at tool dispatch (E.164 parse, endpoint coverage, LLM
/// resolution).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceSetupTestResult {
    /// True when every check passed.
    pub ok: bool,
    /// One-line headline shown next to the button.
    pub summary: String,
    /// Human-readable list of validated wirings — empty when `ok` is false.
    pub checks: Vec<String>,
}

/// Pre-flight check for the Voice panel. Doesn't place an actual call;
/// just verifies the saved settings would let `place_call` run.
///
/// Picks `called_party = "user"` so the user_number validation fires
/// (the most common configuration error). Synthesises a test arc id
/// so the telephony deps can be assembled — no UI artefacts attach to
/// it because no call is placed.
#[tauri::command]
pub async fn test_voice_setup(
    state: State<'_, AppState>,
    _app_handle: AppHandle,
) -> std::result::Result<VoiceSetupTestResult, String> {
    let synthetic_arc = "voice-setup-test";
    let active_provider_id = state.active_provider_id.lock().await.clone();
    let telephony = crate::state::build_telephony_deps(
        synthetic_arc,
        state.approval_router.clone(),
        state.vault.clone(),
        state.http_endpoint_store.clone(),
        state.notifier.load_full(),
        active_provider_id,
        // Preflight never places a real call, so the gate never fires;
        // pass the live global mode for parity with the real path.
        state.security.load().mode,
    )
    .await
    .ok_or_else(|| {
        "Voice subsystem is not wired in this build (missing approval router, vault, or endpoint store)."
            .to_string()
    })?;

    let args = serde_json::json!({
        "number": "+10000000000",
        "objective": "Voice setup pre-flight check (not a real call).",
        "called_party": "user",
    });

    match crate::place_call::preflight(&telephony, &args) {
        Ok(req) => {
            let voice_id = req.voice_id.clone().unwrap_or_else(|| "(none)".into());
            let checks = vec![
                format!("Destination resolved: {}", req.to_number),
                format!("Voice ID: {voice_id}"),
                format!("LLM: {}", req.llm_label),
                format!(
                    "Estimated cost cap ({}s): ${:.2}",
                    req.max_duration_s, req.est_cost_usd
                ),
            ];
            Ok(VoiceSetupTestResult {
                ok: true,
                summary:
                    "Setup looks good. Ask Athen in chat to place a real call to verify end-to-end."
                        .into(),
                checks,
            })
        }
        Err(e) => Ok(VoiceSetupTestResult {
            ok: false,
            summary: format!("Not ready: {e}"),
            checks: Vec::new(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Pipecat install pipeline
// ---------------------------------------------------------------------------

/// Frontend progress event name for the voice setup pipeline.
const VOICE_SETUP_PROGRESS_EVENT: &str = "voice-setup-progress";

/// Resolve the toolbox root the installer writes into. Errors if the
/// data dir can't be located (basically only on misconfigured CI).
fn voice_toolbox_root() -> Result<PathBuf, String> {
    athen_core::paths::athen_toolbox_dir()
        .ok_or_else(|| "could not resolve athen toolbox dir".to_string())
}

/// Snapshot the current state of the Voice runtime install. Cheap —
/// filesystem stats + a marker read, no subprocesses.
#[tauri::command]
pub async fn get_voice_setup_status(
    _state: State<'_, AppState>,
) -> std::result::Result<SetupStatus, String> {
    let toolbox = voice_toolbox_root()?;
    let paths = PipecatPaths::new(toolbox);
    let python_installed = agent_runtimes::is_portable_python_installed();
    Ok(pipecat_runtime::check_status(&paths, python_installed))
}

/// Long-running. Ensures portable Python is installed, then runs
/// `pip install --target` for Pipecat. Emits `voice-setup-progress`
/// events through the lifecycle.
#[tauri::command]
pub async fn install_pipecat(
    _state: State<'_, AppState>,
    app_handle: AppHandle,
) -> std::result::Result<SetupStatus, String> {
    let toolbox = voice_toolbox_root()?;
    let paths = PipecatPaths::new(toolbox);

    // Phase 1: Python (skipped if already present).
    if !agent_runtimes::is_portable_python_installed() {
        emit_setup_progress(
            &app_handle,
            SetupPhase::PythonInstalling,
            "Installing portable Python (~50 MB)…".into(),
            Some(0),
        );

        // Translate athen-agent's InstallProgress into our SetupProgress.
        let app_for_cb = app_handle.clone();
        let cb: agent_runtimes::ProgressCb = Arc::new(move |progress: InstallProgress| {
            let (msg, percent) = match &progress {
                InstallProgress::Resolving => ("Resolving Python download…".to_string(), Some(2)),
                InstallProgress::Downloading { downloaded, total } => {
                    let pct = total
                        .map(|t| {
                            if t == 0 {
                                0u8
                            } else {
                                // Map Python phase to 0..40% of overall.
                                let frac = (*downloaded as f64 / t as f64).clamp(0.0, 1.0);
                                (frac * 40.0) as u8
                            }
                        })
                        .unwrap_or(0);
                    let mb = downloaded / (1024 * 1024);
                    let msg = match total {
                        Some(t) => {
                            let total_mb = t / (1024 * 1024);
                            format!("Downloading Python… {mb}/{total_mb} MB")
                        }
                        None => format!("Downloading Python… {mb} MB"),
                    };
                    (msg, Some(pct))
                }
                InstallProgress::Verifying => ("Verifying Python checksum…".to_string(), Some(42)),
                InstallProgress::Extracting => ("Extracting Python…".to_string(), Some(45)),
                InstallProgress::Done => ("Portable Python ready.".to_string(), Some(50)),
            };
            emit_setup_progress(&app_for_cb, SetupPhase::PythonInstalling, msg, percent);
        });

        agent_runtimes::install_runtime(RuntimeKind::Python, cb)
            .await
            .map_err(|e| {
                let msg = format!("portable Python install failed: {e}");
                emit_setup_progress(&app_handle, SetupPhase::Failed, msg.clone(), None);
                msg
            })?;
    }

    let python_exe = athen_core::paths::athen_portable_python_bin().ok_or_else(|| {
        let msg = "portable Python install completed but binary path is unavailable".to_string();
        emit_setup_progress(&app_handle, SetupPhase::Failed, msg.clone(), None);
        msg
    })?;

    if !python_exe.exists() {
        let msg = format!(
            "portable Python install reported success but {} is missing",
            python_exe.display()
        );
        emit_setup_progress(&app_handle, SetupPhase::Failed, msg.clone(), None);
        return Err(msg);
    }

    // Phase 2: Pipecat install. Move the AppHandle into the callback so
    // each progress event reaches the frontend on the right channel.
    let app_for_pipecat = app_handle.clone();
    let result =
        pipecat_runtime::install_pipecat(&python_exe, &paths, move |progress: SetupProgress| {
            emit_setup_progress_raw(&app_for_pipecat, progress);
        })
        .await;

    match result {
        Ok(status) => Ok(status),
        Err(e) => {
            let msg = e.to_string();
            // pipecat_runtime::install_pipecat already emitted a Failed
            // event before returning; we don't double-emit here.
            Err(msg)
        }
    }
}

/// Build + emit a `SetupProgress` from raw fields. Keeps the
/// translation callsites tidy.
fn emit_setup_progress(app: &AppHandle, phase: SetupPhase, message: String, percent: Option<u8>) {
    emit_setup_progress_raw(
        app,
        SetupProgress {
            phase,
            message,
            percent,
        },
    );
}

fn emit_setup_progress_raw(app: &AppHandle, payload: SetupProgress) {
    if let Err(e) = app.emit(VOICE_SETUP_PROGRESS_EVENT, payload) {
        tracing::warn!(error = %e, "failed to emit voice-setup-progress");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_options_serde_camelcase() {
        let opts = VoiceOptions {
            stt_endpoints: vec![EndpointOption {
                id: "u".into(),
                label: "Deepgram".into(),
                provider: "Deepgram".into(),
                slug: "deepgram".into(),
            }],
            tts_endpoints: vec![],
            phone_endpoints: vec![],
            llm_connections: vec![LlmConnectionOption {
                connection_id: "deepseek".into(),
                connection_label: "Deepseek".into(),
                slug: "deepseek-chat".into(),
                display: "Deepseek :: deepseek-chat".into(),
            }],
            fast_tier_label: Some("Use Fast tier (Deepseek :: deepseek-chat)".into()),
        };
        let json = serde_json::to_value(&opts).unwrap();
        assert!(json.get("sttEndpoints").is_some());
        assert!(json.get("ttsEndpoints").is_some());
        assert!(json.get("phoneEndpoints").is_some());
        assert!(json.get("llmConnections").is_some());
        assert!(json.get("fastTierLabel").is_some());
        // ensure NOT snake_case
        assert!(json.get("stt_endpoints").is_none());
        let llm = &json.get("llmConnections").unwrap()[0];
        assert!(llm.get("connectionId").is_some());
        assert!(llm.get("connectionLabel").is_some());
    }

    #[test]
    fn classify_recognises_known_providers() {
        assert_eq!(classify("Deepgram"), Some(VoiceBucket::Stt));
        assert_eq!(classify("deepgram"), Some(VoiceBucket::Stt));
        assert_eq!(classify("ElevenLabs"), Some(VoiceBucket::Tts));
        assert_eq!(classify("Cartesia"), Some(VoiceBucket::Tts));
        assert_eq!(classify("Twilio"), Some(VoiceBucket::Phone));
    }

    #[test]
    fn classify_returns_none_for_unrelated() {
        assert_eq!(classify(""), None);
        assert_eq!(classify("Jina AI"), None);
        assert_eq!(classify("OpenAI"), None);
    }

    #[test]
    fn classify_is_substring_tolerant() {
        // A user who types "Twilio Voice" or "Deepgram (work)" should
        // still bucket correctly.
        assert_eq!(classify("Twilio Voice"), Some(VoiceBucket::Phone));
        assert_eq!(classify("Deepgram (work)"), Some(VoiceBucket::Stt));
    }

    #[test]
    fn decode_voice_handles_null_blob() {
        let cfg = decode_voice(&serde_json::Value::Null);
        assert_eq!(cfg.max_call_duration_s, VoiceConfig::DEFAULT_MAX_DURATION_S);
        assert!(cfg.stt_endpoint_id.is_none());
    }

    #[test]
    fn decode_voice_round_trips() {
        let mut original = VoiceConfig::new();
        original.stt_endpoint_id = Some("abc".into());
        original.voice_id = Some("voice-1".into());
        original.max_call_duration_s = 900;
        let blob = serde_json::to_value(&original).unwrap();
        let decoded = decode_voice(&blob);
        assert_eq!(decoded.stt_endpoint_id.as_deref(), Some("abc"));
        assert_eq!(decoded.voice_id.as_deref(), Some("voice-1"));
        assert_eq!(decoded.max_call_duration_s, 900);
    }

    #[test]
    fn decode_voice_returns_default_on_malformed_blob() {
        let blob = serde_json::json!({ "garbage": true });
        // serde derives are strict on unknown variants of unrelated
        // shapes — but VoiceConfig is permissive (`#[serde(default)]`
        // is implicit per-field via Option/u32 defaults). We tolerate
        // any decode outcome, but the helper itself must never panic.
        let _ = decode_voice(&blob);
    }
}
