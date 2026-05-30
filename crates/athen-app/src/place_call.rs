//! Production handler for the `place_call` agent tool.
//!
//! Wires the user-visible tool to:
//! 1. Voice config + minimal-configuration check.
//! 2. E.164 validation + duration clamp.
//! 3. LLM resolution (per-arc pinning aware, with optional override).
//! 4. STT / TTS / Phone credential extraction from registered HTTP endpoints.
//! 5. Cost estimation + approval-gate prompt.
//! 6. Runner-script extraction + Pipecat install (idempotent).
//! 7. Subprocess spawn with streaming jsonl stdout.
//! 8. Tauri `place-call-progress` event emission for live UI updates.
//! 9. Final result + completion notification.
//!
//! See `crates/athen-app/src/voice.rs` for the Settings panel commands;
//! the install pipeline (batches 1C/2A/2B) lives in `athen-voice`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use athen_core::config::{AthenConfig, AuthType, Bundle, ACTIVE_BUNDLE_KEY};
use athen_core::error::{AthenError, Result};
use athen_core::http_endpoint::{AuthMethod, RegisteredEndpoint};
use athen_core::llm::ModelFamily;
use athen_core::llm::ModelProfile;
use athen_core::notification::{Notification, NotificationOrigin, NotificationUrgency};
use athen_core::tool::ToolResult;
use athen_core::traits::http_endpoint::HttpEndpointStore;
use athen_core::traits::vault::Vault;
use athen_persistence::http_endpoints::SqliteHttpEndpointStore;
use athen_voice::{
    estimate_call_cost_usd, validate_e164, CallRequest, CalledParty, TelephonyApprovalGate,
    VoiceConfig,
};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use uuid::Uuid;

use crate::notifier::NotificationOrchestrator;
use crate::vault_creds::endpoint_scope;

/// Frontend event name carrying live progress for an in-flight call.
const PROGRESS_EVENT: &str = "place-call-progress";

/// Wall-clock grace window past `max_duration_s` before we kill the
/// runner ourselves. Pipecat's own timeout fires first under normal
/// conditions — this is the "stuck in TwilioClient.create" backstop.
const GRACE_SECS: u64 = 30;

// ---------------------------------------------------------------------------
// Public deps surface
// ---------------------------------------------------------------------------

/// Dependencies the place_call handler reads from. Built once by the
/// composition root and cloned into each per-arc registry via
/// [`crate::app_tools::AppToolRegistry::with_telephony`].
#[derive(Clone)]
pub struct TelephonyDeps {
    pub gate: Arc<dyn TelephonyApprovalGate>,
    pub vault: Arc<dyn Vault>,
    pub http_endpoint_store: Arc<SqliteHttpEndpointStore>,
    pub notifier: Option<Arc<NotificationOrchestrator>>,
    pub active_provider_id: String,
    /// Persisted [`VoiceConfig`] blob, deserialized.
    pub voice_config: VoiceConfig,
    /// Full config — needed to resolve the LLM connection (api_key,
    /// base_url, family) from `models.providers`.
    pub config: AthenConfig,
}

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

pub fn place_call_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "number": {
                "type": "string",
                "description": "Phone number in E.164 format, e.g. +14155551234. Ignored when called_party='user'."
            },
            "objective": {
                "type": "string",
                "description": "What to accomplish on the call. Be specific (e.g. 'Book a table for 4 at 8pm tomorrow under the name Alex')."
            },
            "called_party": {
                "type": "string",
                "enum": ["user", "other"],
                "default": "other",
                "description": "'user' = calling the user themselves for a reminder; 'other' = calling someone else on the user's behalf."
            },
            "voice_id": {
                "type": "string",
                "description": "Optional override of the configured voice ID for this call."
            },
            "max_duration_s": {
                "type": "integer",
                "minimum": 30,
                "maximum": 1800,
                "default": 600,
                "description": "Hard cap on call length in seconds."
            }
        },
        "required": ["number", "objective"]
    })
}

pub const PLACE_CALL_DESCRIPTION: &str = "Place an outbound phone call. Athen dials the number, talks to whoever answers, pursues the objective, and returns a transcript + outcome. Requires user approval. Use for: making reservations, gathering information that requires a human conversation, delivering important reminders to the user themselves (called_party='user').";

// ---------------------------------------------------------------------------
// Parsed args
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ParsedArgs {
    raw_number: String,
    objective: String,
    called_party: CalledParty,
    voice_id_override: Option<String>,
    max_duration_s: u32,
}

fn parse_args(args: &Value) -> Result<ParsedArgs> {
    let raw_number = args
        .get("number")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AthenError::Other("place_call: 'number' is required".into()))?
        .to_string();
    let objective = args
        .get("objective")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AthenError::Other("place_call: 'objective' is required".into()))?
        .to_string();
    let called_party = match args.get("called_party").and_then(|v| v.as_str()) {
        Some("user") => CalledParty::User,
        Some("other") | None => CalledParty::Other,
        Some(other) => {
            return Err(AthenError::Other(format!(
                "place_call: called_party must be 'user' or 'other', got '{other}'"
            )))
        }
    };
    let voice_id_override = args
        .get("voice_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Duration: default 600, clamp to VoiceConfig::HARD_MAX_DURATION_S.
    let raw_dur = args
        .get("max_duration_s")
        .and_then(|v| v.as_u64())
        .unwrap_or(VoiceConfig::DEFAULT_MAX_DURATION_S as u64) as u32;
    let (max_duration_s, _) = VoiceConfig::clamp_duration(raw_dur);

    Ok(ParsedArgs {
        raw_number,
        objective,
        called_party,
        voice_id_override,
        max_duration_s,
    })
}

// ---------------------------------------------------------------------------
// LLM resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct ResolvedLlm {
    pub kind: PipecatLlmKind,
    pub api_key: String,
    pub base_url: Option<String>,
    pub model_slug: String,
    pub llm_label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PipecatLlmKind {
    OpenAiCompat,
    Anthropic,
    Google,
}

impl PipecatLlmKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompat => "openai_compat",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
        }
    }
}

/// Map a connection's [`ModelFamily`] (or, lacking that, the
/// connection id heuristic) to the Pipecat LLM kind.
pub(crate) fn pipecat_kind_for(connection_id: &str, family: ModelFamily) -> Option<PipecatLlmKind> {
    // Family is authoritative when set to something specific. Pattern
    // match on the Debug label so we don't have to enumerate the
    // dozen-plus variants (they're per-model, not per-vendor).
    let family_label = format!("{family:?}").to_ascii_lowercase();
    if family_label.contains("claude") {
        return Some(PipecatLlmKind::Anthropic);
    }
    if family_label.contains("gemini") {
        return Some(PipecatLlmKind::Google);
    }
    // Every other variant (deepseek, qwen, gpt, kimi, mistral, llama, …)
    // talks OpenAI-compatible JSON, which is what Pipecat's
    // `openai_compat` adapter expects.
    if !matches!(family, ModelFamily::Default) {
        return Some(PipecatLlmKind::OpenAiCompat);
    }
    // Fallback: id heuristic — covers the case where a user has a
    // provider entry but never edited the family dropdown.
    let id = connection_id.to_ascii_lowercase();
    if id.contains("anthropic") || id.contains("claude") {
        Some(PipecatLlmKind::Anthropic)
    } else if id.contains("google") || id.contains("gemini") {
        Some(PipecatLlmKind::Google)
    } else {
        // Unknown — default to openai_compat which is the most
        // permissive wire format. Worst case the runner surfaces a
        // clear error and the user picks a different connection.
        Some(PipecatLlmKind::OpenAiCompat)
    }
}

/// Pick the (connection_id, slug) the call should run on. Honours the
/// per-call override in [`VoiceConfig`]; otherwise pulls the Fast tier
/// from the active Bundle, falling back to the active provider's
/// `default_model`.
pub(crate) fn pick_voice_llm(
    cfg: &AthenConfig,
    voice: &VoiceConfig,
    active_id: &str,
) -> (String, String) {
    if let (Some(cid), Some(slug)) = (
        voice.llm_override_connection_id.as_deref(),
        voice.llm_override_slug.as_deref(),
    ) {
        if cfg.models.providers.contains_key(cid) {
            return (cid.to_string(), slug.to_string());
        }
    }
    // Fast tier of active Bundle.
    if let Some(bundle_id) = cfg.models.assignments.get(ACTIVE_BUNDLE_KEY) {
        if let Some(bundle) = cfg.models.bundles.get(bundle_id) {
            if let Some(tier) = pick_tier(bundle, ModelProfile::Fast) {
                if cfg.models.providers.contains_key(&tier.connection_id) {
                    return (tier.connection_id.clone(), tier.slug.clone());
                }
            }
        }
    }
    // Legacy fallback.
    if let Some(p) = cfg.models.providers.get(active_id) {
        let slug = p
            .tier_models
            .get(&ModelProfile::Fast)
            .cloned()
            .unwrap_or_else(|| p.default_model.clone());
        return (active_id.to_string(), slug);
    }
    (active_id.to_string(), String::new())
}

/// Walk the bundle tier-fallback ladder Fast → Cheap.
fn pick_tier(bundle: &Bundle, tier: ModelProfile) -> Option<&athen_core::config::BundleTier> {
    match tier {
        ModelProfile::Fast => bundle
            .tiers
            .get(&ModelProfile::Fast)
            .or_else(|| bundle.tiers.get(&ModelProfile::Cheap)),
        _ => bundle.tiers.get(&tier),
    }
}

fn resolve_llm(deps: &TelephonyDeps) -> Result<ResolvedLlm> {
    let (connection_id, slug) =
        pick_voice_llm(&deps.config, &deps.voice_config, &deps.active_provider_id);
    let provider = deps
        .config
        .models
        .providers
        .get(&connection_id)
        .ok_or_else(|| AthenError::Other(format!(
            "place_call: voice LLM connection '{connection_id}' is not configured. Pick a different connection in Settings → Voice."
        )))?;
    let api_key = match &provider.auth {
        AuthType::ApiKey(k) => k.trim().to_string(),
        AuthType::None | AuthType::OAuth => String::new(),
    };
    if api_key.is_empty() {
        return Err(AthenError::Other(format!(
            "place_call: voice LLM connection '{connection_id}' has no API key. Set it in Settings → Connections."
        )));
    }
    let kind = pipecat_kind_for(&connection_id, provider.family).ok_or_else(|| {
        AthenError::Other(format!(
            "place_call: voice LLM provider '{connection_id}' is not supported by Pipecat — pick a different connection in Settings → Voice (currently supported families: openai-compatible, anthropic, google)."
        ))
    })?;
    let model_slug = if slug.is_empty() {
        provider.default_model.clone()
    } else {
        slug
    };
    // Prefer the per-connection endpoint override; otherwise fall back to
    // the preset default base URL for this provider id. Preset providers
    // (e.g. opencode_go → https://opencode.ai/zen/go) leave `endpoint`
    // empty in config — the in-process router fills it from this same
    // table, but Pipecat runs out-of-process and needs the URL spelled
    // out, or it defaults to api.openai.com and the call fails.
    let base_url = provider
        .endpoint
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            let preset = crate::settings::default_base_url(&connection_id);
            (!preset.is_empty()).then(|| preset.to_string())
        });
    let label_prefix = display_connection_label(&connection_id);
    Ok(ResolvedLlm {
        kind,
        api_key,
        base_url,
        model_slug: model_slug.clone(),
        llm_label: format!("{label_prefix} :: {model_slug}"),
    })
}

fn display_connection_label(id: &str) -> String {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Endpoint credential resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StackCreds {
    stt_kind: String,
    stt_api_key: String,
    tts_kind: String,
    tts_api_key: String,
    phone_account_sid: String,
    phone_auth_token: String,
    phone_provider_label: String,
    stt_provider_label: String,
    tts_provider_label: String,
}

async fn load_endpoint(
    store: &SqliteHttpEndpointStore,
    id_str: &str,
    role: &str,
) -> Result<RegisteredEndpoint> {
    let id = Uuid::parse_str(id_str).map_err(|e| {
        AthenError::Other(format!("place_call: {role} endpoint id is not a UUID: {e}"))
    })?;
    store.get(id).await?.ok_or_else(|| {
        AthenError::Other(format!(
            "place_call: {role} endpoint not found — open Settings → Voice and re-pick."
        ))
    })
}

async fn resolve_stack(
    vault: &Arc<dyn Vault>,
    store: &SqliteHttpEndpointStore,
    voice: &VoiceConfig,
) -> Result<StackCreds> {
    let stt_id = voice
        .stt_endpoint_id
        .as_deref()
        .ok_or_else(|| AthenError::Other("place_call: STT endpoint not configured".into()))?;
    let tts_id = voice
        .tts_endpoint_id
        .as_deref()
        .ok_or_else(|| AthenError::Other("place_call: TTS endpoint not configured".into()))?;
    let phone_id = voice
        .phone_endpoint_id
        .as_deref()
        .ok_or_else(|| AthenError::Other("place_call: phone endpoint not configured".into()))?;

    let stt = load_endpoint(store, stt_id, "STT").await?;
    let tts = load_endpoint(store, tts_id, "TTS").await?;
    let phone = load_endpoint(store, phone_id, "phone").await?;

    let stt_api_key = vault
        .get(&endpoint_scope(stt.id), "token")
        .await?
        .or(vault.get(&endpoint_scope(stt.id), "value").await?)
        .unwrap_or_default();
    if stt_api_key.is_empty() {
        return Err(AthenError::Other(format!(
            "place_call: STT endpoint '{}' has no credential in the vault. Open Settings → Cloud APIs to set it.",
            stt.name
        )));
    }
    let tts_api_key = vault
        .get(&endpoint_scope(tts.id), "token")
        .await?
        .or(vault.get(&endpoint_scope(tts.id), "value").await?)
        .unwrap_or_default();
    if tts_api_key.is_empty() {
        return Err(AthenError::Other(format!(
            "place_call: TTS endpoint '{}' has no credential in the vault.",
            tts.name
        )));
    }

    // Twilio: BasicAuth user is the account SID; password is the auth token.
    let (account_sid, auth_token) = match &phone.auth_method {
        AuthMethod::BasicAuth { user } => {
            let token = vault
                .get(&endpoint_scope(phone.id), "password")
                .await?
                .unwrap_or_default();
            if token.is_empty() {
                return Err(AthenError::Other(format!(
                    "place_call: phone endpoint '{}' has no password (Twilio auth token) in the vault.",
                    phone.name
                )));
            }
            (user.clone(), token)
        }
        _ => {
            return Err(AthenError::Other(format!(
                "place_call: phone endpoint '{}' must use BasicAuth (Twilio account SID + auth token).",
                phone.name
            )));
        }
    };

    Ok(StackCreds {
        stt_kind: classify_kind(&stt.provider, &["deepgram"], "deepgram"),
        stt_api_key,
        tts_kind: classify_kind(&tts.provider, &["elevenlabs", "cartesia"], "elevenlabs"),
        tts_api_key,
        phone_account_sid: account_sid,
        phone_auth_token: auth_token,
        phone_provider_label: phone.provider.clone(),
        stt_provider_label: stt.provider.clone(),
        tts_provider_label: tts.provider.clone(),
    })
}

fn classify_kind(provider: &str, known: &[&str], fallback: &str) -> String {
    let p = provider.trim().to_ascii_lowercase();
    for k in known {
        if p == *k || p.contains(k) {
            return (*k).to_string();
        }
    }
    fallback.to_string()
}

// ---------------------------------------------------------------------------
// Runner spawn + jsonl streaming
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
struct TranscriptEntry {
    speaker: String,
    text: String,
    ts: Option<f64>,
}

#[derive(Debug)]
struct RunOutcome {
    transcript: Vec<TranscriptEntry>,
    result_event: Option<Value>,
    stderr_tail: Vec<String>,
}

async fn run_pipecat(
    app_handle: &AppHandle,
    arc_id: &str,
    python_exe: &PathBuf,
    runner_script: &PathBuf,
    pipecat_env: &PathBuf,
    config_path: &PathBuf,
    max_duration_s: u32,
) -> std::result::Result<RunOutcome, String> {
    let mut cmd = Command::new(python_exe);
    cmd.arg(runner_script)
        .arg("--config-file")
        .arg(config_path)
        .env("PYTHONPATH", pipecat_env)
        .env_remove("PYTHONHOME")
        .env("PYTHONUNBUFFERED", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn pipecat_runner: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "pipecat_runner: no stdout pipe".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "pipecat_runner: no stderr pipe".to_string())?;

    // stderr drain — buffer the tail so we can include it on failure.
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut buf: Vec<String> = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target = "place_call.stderr", "{line}");
            buf.push(line);
            if buf.len() > 2000 {
                buf.drain(..1000);
            }
        }
        buf
    });

    let mut transcript: Vec<TranscriptEntry> = Vec::new();
    let mut result_event: Option<Value> = None;

    let stdout_reader = BufReader::new(stdout);
    let mut lines = stdout_reader.lines();

    // Hard wall-clock cap. Pipecat's own timeout should fire first; this
    // is the backstop for "stuck before any audio".
    let deadline = Instant::now() + Duration::from_secs(max_duration_s as u64 + GRACE_SECS);
    loop {
        let remaining = deadline.checked_duration_since(Instant::now());
        let Some(remaining) = remaining else {
            // Hit our backstop.
            let _ = child.start_kill();
            let _ = child.wait().await;
            let stderr_buf = stderr_task.await.unwrap_or_default();
            return Err(format!(
                "place_call: hard timeout after {}s — killed runner. Last stderr line: {}",
                max_duration_s as u64 + GRACE_SECS,
                stderr_buf.last().cloned().unwrap_or_default()
            ));
        };
        match tokio::time::timeout(remaining, lines.next_line()).await {
            Ok(Ok(Some(line))) => {
                if line.trim().is_empty() {
                    continue;
                }
                let parsed: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => {
                        tracing::debug!(target = "place_call", "non-jsonl stdout: {line}");
                        continue;
                    }
                };
                let event = parsed.get("event").and_then(|v| v.as_str()).unwrap_or("");
                emit_progress(app_handle, arc_id, &parsed);
                match event {
                    "transcript" => {
                        let speaker = parsed
                            .get("speaker")
                            .and_then(|v| v.as_str())
                            .unwrap_or("agent")
                            .to_string();
                        let text = parsed
                            .get("text")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string();
                        let ts = parsed.get("ts").and_then(|v| v.as_f64());
                        transcript.push(TranscriptEntry { speaker, text, ts });
                    }
                    "result" => {
                        result_event = Some(parsed);
                        break;
                    }
                    _ => {}
                }
            }
            Ok(Ok(None)) => break, // stdout EOF
            Ok(Err(e)) => {
                return Err(format!("place_call: stdout read error: {e}"));
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                let stderr_buf = stderr_task.await.unwrap_or_default();
                return Err(format!(
                    "place_call: hard timeout after {}s — killed runner. Last stderr line: {}",
                    max_duration_s as u64 + GRACE_SECS,
                    stderr_buf.last().cloned().unwrap_or_default()
                ));
            }
        }
    }

    // Wait for the child to actually exit. If a result event came in, we
    // give it a short grace; otherwise we already broke on EOF.
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let stderr_tail = stderr_task.await.unwrap_or_default();

    Ok(RunOutcome {
        transcript,
        result_event,
        stderr_tail,
    })
}

fn emit_progress(app: &AppHandle, arc_id: &str, event: &Value) {
    let payload = json!({
        "arc_id": arc_id,
        "event": event,
    });
    if let Err(e) = app.emit(PROGRESS_EVENT, &payload) {
        tracing::debug!(error = %e, "failed to emit place-call-progress");
    }
}

// ---------------------------------------------------------------------------
// Top-level handler
// ---------------------------------------------------------------------------

/// Entry point for the place_call tool. Lives outside `app_tools.rs` to
/// keep that file from growing unbounded.
pub async fn do_place_call(
    deps: &TelephonyDeps,
    app_handle: &AppHandle,
    arc_id: &str,
    args: Value,
) -> Result<ToolResult> {
    let started = Instant::now();
    let parsed = parse_args(&args)?;

    // 1. Resolve the destination number per called_party.
    let voice = &deps.voice_config;
    let destination = match parsed.called_party {
        CalledParty::User => voice
            .user_number
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                AthenError::Other(
                    "place_call: called_party='user' but no user_number is set in Settings → Voice."
                        .into(),
                )
            })?,
        CalledParty::Other => parsed.raw_number.clone(),
    };
    validate_e164(&destination).map_err(|e| AthenError::Other(format!("place_call: {e}")))?;

    // 2. Minimal-config check.
    if !voice.is_minimally_configured() {
        let missing: Vec<&'static str> = voice
            .missing_endpoints()
            .iter()
            .map(|m| m.label())
            .collect();
        return Err(AthenError::Other(format!(
            "place_call: voice not set up — missing {}. Open Settings → Voice to wire it.",
            missing.join(", ")
        )));
    }
    let from_number = voice
        .from_number
        .clone()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            AthenError::Other(
                "place_call: Voice settings is missing the 'from' number. Open Settings → Voice."
                    .into(),
            )
        })?;

    // 3. LLM resolution.
    let llm = resolve_llm(deps)?;

    // 4. STT/TTS/Phone credentials.
    let stack = resolve_stack(&deps.vault, &deps.http_endpoint_store, voice).await?;

    // 5. Cost estimate.
    let est = estimate_call_cost_usd(parsed.max_duration_s);

    // 6. Voice ID (override beats configured).
    let effective_voice_id = parsed
        .voice_id_override
        .clone()
        .or_else(|| voice.voice_id.clone())
        .filter(|s| !s.trim().is_empty());
    let voice_id_for_runner = effective_voice_id.clone().ok_or_else(|| {
        AthenError::Other(
            "place_call: no voice_id available — set one in Settings → Voice or pass it as an argument."
                .into(),
        )
    })?;

    // 7. Approval gate.
    let arc_uuid = Uuid::parse_str(arc_id).unwrap_or_else(|_| Uuid::new_v4());
    let request = CallRequest {
        arc_id: arc_uuid,
        to_number: destination.clone(),
        objective: parsed.objective.clone(),
        called_party: parsed.called_party,
        voice_id: effective_voice_id.clone(),
        max_duration_s: parsed.max_duration_s,
        est_cost_usd: est,
        llm_label: llm.llm_label.clone(),
        voice_provider: stack.tts_provider_label.clone(),
        stt_provider: stack.stt_provider_label.clone(),
        phone_provider: stack.phone_provider_label.clone(),
    };
    let approved = deps.gate.confirm_call(&request).await;
    if !approved {
        return Ok(ToolResult {
            success: false,
            output: json!({
                "denied": true,
                "to_number": destination,
                "objective": parsed.objective,
            }),
            error: Some("User denied the place_call request.".into()),
            execution_time_ms: started.elapsed().as_millis() as u64,
        });
    }

    // 8. Ensure runner script + pipecat env are present.
    let runner_script = crate::voice::ensure_runner_extracted(app_handle)
        .map_err(|e| AthenError::Other(format!("place_call: {e}")))?;

    let toolbox = athen_core::paths::athen_toolbox_dir()
        .ok_or_else(|| AthenError::Other("place_call: athen toolbox dir is unavailable".into()))?;
    let paths = athen_voice::pipecat_runtime::PipecatPaths::new(toolbox);
    let python_installed = athen_agent::runtimes::is_portable_python_installed();
    let status = athen_voice::pipecat_runtime::check_status(&paths, python_installed);
    if !status.pipecat_installed {
        return Err(AthenError::Other(
            "place_call: Pipecat runtime is not installed. Open Settings → Voice and click 'Install Voice runtime' before trying again.".into(),
        ));
    }
    let python_exe = athen_core::paths::athen_portable_python_bin()
        .filter(|p| p.exists())
        .ok_or_else(|| {
            AthenError::Other(
                "place_call: portable Python binary is missing — reinstall runtimes from Settings → Voice."
                    .into(),
            )
        })?;

    // 9. Build the runner config JSON.
    let mut llm_block = json!({
        "type": llm.kind.as_str(),
        "api_key": llm.api_key,
        "model": llm.model_slug,
    });
    if let Some(base) = &llm.base_url {
        if matches!(llm.kind, PipecatLlmKind::OpenAiCompat) {
            llm_block
                .as_object_mut()
                .unwrap()
                .insert("base_url".into(), json!(base));
        }
    }
    let mut config_blob = json!({
        "number": destination,
        "objective": parsed.objective,
        "called_party": match parsed.called_party {
            CalledParty::User => "user",
            CalledParty::Other => "other",
        },
        "voice_persona_prefix": "",
        "llm": llm_block,
        "stt": {
            "type": stack.stt_kind,
            "api_key": stack.stt_api_key,
        },
        "tts": {
            "type": stack.tts_kind,
            "api_key": stack.tts_api_key,
            "voice_id": voice_id_for_runner,
        },
        "phone": {
            "type": "twilio",
            "account_sid": stack.phone_account_sid,
            "auth_token": stack.phone_auth_token,
            "from_number": from_number,
        },
        "max_duration_s": parsed.max_duration_s,
    });
    // Public-URL reachability for Twilio Media Streams. The runner needs
    // one of these or it aborts with "no public URL available". Only inject
    // when set so the runner's own priority (public_url > ngrok) holds.
    {
        let obj = config_blob.as_object_mut().expect("config_blob is object");
        if let Some(url) = deps
            .voice_config
            .public_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            obj.insert("public_url".into(), json!(url));
        }
        if let Some(tok) = deps
            .voice_config
            .ngrok_authtoken
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            obj.insert("ngrok_authtoken".into(), json!(tok));
        }
    }

    // 10. Persist to a NamedTempFile so the file is auto-deleted on drop.
    //     We hold it alive until after the child exits.
    let mut tf = tempfile::Builder::new()
        .prefix("athen-voice-")
        .suffix(".json")
        .tempfile()
        .map_err(|e| AthenError::Other(format!("place_call: cannot create temp config: {e}")))?;
    {
        use std::io::Write;
        let body = serde_json::to_vec_pretty(&config_blob)
            .map_err(|e| AthenError::Other(format!("place_call: serialize config: {e}")))?;
        tf.write_all(&body)
            .and_then(|_| tf.flush())
            .map_err(|e| AthenError::Other(format!("place_call: write temp config: {e}")))?;
    }
    let temp_config_path = tf.path().to_path_buf();

    // 11. Spawn + stream.
    emit_progress(
        app_handle,
        arc_id,
        &json!({ "event": "starting", "to": destination }),
    );
    let outcome = match run_pipecat(
        app_handle,
        arc_id,
        &python_exe,
        &runner_script,
        &paths.pipecat_env(),
        &temp_config_path,
        parsed.max_duration_s,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            // Surface the failure as a tool error + notification.
            if let Some(n) = &deps.notifier {
                fire_notification(n, arc_id, &format!("Call to {destination} failed"), &e).await;
            }
            // Drop tempfile here implicitly when `tf` falls out of scope.
            drop(tf);
            return Ok(ToolResult {
                success: false,
                output: json!({
                    "to_number": destination,
                    "objective": parsed.objective,
                    "error": e,
                }),
                error: Some(e),
                execution_time_ms: started.elapsed().as_millis() as u64,
            });
        }
    };

    // Tempfile gets dropped (and deleted) right after this scope exits.
    drop(tf);

    // 12. Build the final ToolResult from the runner's `result` event,
    //     falling back to a synthesized envelope when the runner exited
    //     without emitting one (rare; pipecat_runner.py always emits one
    //     on the happy paths).
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let mut transcript_json = Vec::with_capacity(outcome.transcript.len());
    for t in &outcome.transcript {
        let mut row = json!({
            "speaker": t.speaker,
            "text": t.text,
        });
        if let Some(ts) = t.ts {
            row.as_object_mut().unwrap().insert("ts".into(), json!(ts));
        }
        transcript_json.push(row);
    }

    if let Some(result_event) = outcome.result_event {
        let outcome_str = result_event
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("unclear")
            .to_string();
        let duration_s = result_event
            .get("duration_s")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let cost = result_event
            .get("cost_estimate_usd")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let summary = result_event
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let success = !matches!(outcome_str.as_str(), "error" | "failed" | "timeout");

        if let Some(n) = &deps.notifier {
            let title = if success {
                format!("Call to {destination} completed")
            } else {
                format!("Call to {destination} failed")
            };
            fire_notification(n, arc_id, &title, &summary).await;
        }

        return Ok(ToolResult {
            success,
            output: json!({
                "to_number": destination,
                "objective": parsed.objective,
                "outcome": outcome_str,
                "duration_s": duration_s,
                "cost_estimate_usd": cost,
                "summary": summary,
                "transcript": transcript_json,
            }),
            error: None,
            execution_time_ms: elapsed_ms,
        });
    }

    // No result event — assemble what we have.
    let stderr_tail = outcome.stderr_tail.join("\n");
    if let Some(n) = &deps.notifier {
        fire_notification(
            n,
            arc_id,
            &format!("Call to {destination} ended"),
            "Call ended without a final result event.",
        )
        .await;
    }
    Ok(ToolResult {
        success: false,
        output: json!({
            "to_number": destination,
            "objective": parsed.objective,
            "outcome": "unclear",
            "duration_s": elapsed_ms / 1000,
            "summary": "Call ended without a result event.",
            "transcript": transcript_json,
            "stderr_tail": stderr_tail,
        }),
        error: Some("Call ended without a final result event.".into()),
        execution_time_ms: elapsed_ms,
    })
}

async fn fire_notification(
    notifier: &Arc<NotificationOrchestrator>,
    arc_id: &str,
    title: &str,
    body: &str,
) {
    let notif = Notification {
        id: Uuid::new_v4(),
        urgency: NotificationUrgency::High,
        title: title.to_string(),
        body: body.chars().take(280).collect(),
        body_long: Some(body.to_string()),
        origin: NotificationOrigin::Agent,
        arc_id: Some(arc_id.to_string()),
        task_id: None,
        created_at: chrono::Utc::now(),
        requires_response: false,
        skip_humanize: true,
    };
    notifier.notify(notif).await;
}

// ---------------------------------------------------------------------------
// Validation-only entrypoint for unit tests.
// ---------------------------------------------------------------------------

/// Run only the pre-flight checks (arg parse, e164 validation,
/// minimally-configured check, LLM resolution) so we can test rejection
/// paths without spawning subprocesses. Returns the synthesized
/// CallRequest the gate would receive on success.
///
/// Also reused by the Settings → Voice "Test setup" button via
/// [`crate::voice::test_voice_setup`] to validate configuration without
/// placing a real call.
pub(crate) fn preflight(deps: &TelephonyDeps, args: &Value) -> Result<CallRequest> {
    let parsed = parse_args(args)?;
    let voice = &deps.voice_config;
    let destination = match parsed.called_party {
        CalledParty::User => voice
            .user_number
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| AthenError::Other("place_call: user_number unset".into()))?,
        CalledParty::Other => parsed.raw_number.clone(),
    };
    validate_e164(&destination).map_err(|e| AthenError::Other(format!("place_call: {e}")))?;
    if !voice.is_minimally_configured() {
        let missing: Vec<&'static str> = voice
            .missing_endpoints()
            .iter()
            .map(|m| m.label())
            .collect();
        return Err(AthenError::Other(format!(
            "place_call: missing {}",
            missing.join(", ")
        )));
    }
    let llm = resolve_llm(deps)?;
    Ok(CallRequest {
        arc_id: Uuid::new_v4(),
        to_number: destination,
        objective: parsed.objective,
        called_party: parsed.called_party,
        voice_id: parsed.voice_id_override.or_else(|| voice.voice_id.clone()),
        max_duration_s: parsed.max_duration_s,
        est_cost_usd: estimate_call_cost_usd(parsed.max_duration_s),
        llm_label: llm.llm_label,
        voice_provider: "(tts)".into(),
        stt_provider: "(stt)".into(),
        phone_provider: "(phone)".into(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use athen_core::config::{AthenConfig, ProviderConfig};
    use athen_persistence::Database;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn mk_provider(api_key: &str, family: ModelFamily) -> ProviderConfig {
        ProviderConfig {
            auth: AuthType::ApiKey(api_key.into()),
            default_model: "deepseek-chat".into(),
            endpoint: Some("https://api.deepseek.com/v1".into()),
            context_window_tokens: 128_000,
            compaction_trigger_pct: 65,
            compaction_target_pct: 30,
            supports_vision: false,
            supports_documents: false,
            family,
            temperature: None,
            tier_models: HashMap::new(),
        }
    }

    async fn mk_deps(
        gate: Arc<dyn TelephonyApprovalGate>,
        voice: VoiceConfig,
        provider_family: ModelFamily,
    ) -> TelephonyDeps {
        // We need a real SqliteHttpEndpointStore + vault for the deps,
        // but the preflight() helper never touches them so any in-memory
        // instance works.
        let db = Database::in_memory().await.unwrap();
        let store = Arc::new(db.http_endpoint_store());

        struct MemoryVault;
        #[async_trait]
        impl Vault for MemoryVault {
            async fn set(
                &self,
                _scope: &str,
                _key: &str,
                _value: &str,
            ) -> athen_core::error::Result<()> {
                Ok(())
            }
            async fn get(
                &self,
                _scope: &str,
                _key: &str,
            ) -> athen_core::error::Result<Option<String>> {
                Ok(None)
            }
            async fn delete(&self, _scope: &str, _key: &str) -> athen_core::error::Result<()> {
                Ok(())
            }
            async fn list(&self, _scope: &str) -> athen_core::error::Result<Vec<String>> {
                Ok(Vec::new())
            }
        }
        let vault: Arc<dyn Vault> = Arc::new(MemoryVault);

        let mut cfg = AthenConfig::default();
        cfg.models
            .providers
            .insert("deepseek".into(), mk_provider("sk-fake", provider_family));
        cfg.models
            .assignments
            .insert("active_provider".into(), "deepseek".into());

        TelephonyDeps {
            gate,
            vault,
            http_endpoint_store: store,
            notifier: None,
            active_provider_id: "deepseek".into(),
            voice_config: voice,
            config: cfg,
        }
    }

    fn populated_voice() -> VoiceConfig {
        let mut v = VoiceConfig::new();
        v.stt_endpoint_id = Some(Uuid::new_v4().to_string());
        v.tts_endpoint_id = Some(Uuid::new_v4().to_string());
        v.phone_endpoint_id = Some(Uuid::new_v4().to_string());
        v.voice_id = Some("rachel".into());
        v.from_number = Some("+14155550199".into());
        v.user_number = Some("+14155550200".into());
        v
    }

    struct ApproveGate;
    #[async_trait]
    impl TelephonyApprovalGate for ApproveGate {
        async fn confirm_call(&self, _r: &CallRequest) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn place_call_rejects_invalid_e164() {
        let deps = mk_deps(
            Arc::new(ApproveGate),
            populated_voice(),
            ModelFamily::Default,
        )
        .await;
        let err = preflight(
            &deps,
            &json!({"number": "not-a-number", "objective": "test"}),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("place_call"), "got: {msg}");
        assert!(
            msg.contains("must start with") || msg.contains("E.164"),
            "got: {msg}"
        );
    }

    #[tokio::test]
    async fn place_call_rejects_when_not_minimally_configured() {
        let mut voice = VoiceConfig::new();
        voice.from_number = Some("+14155550199".into());
        // Leave STT/TTS/Phone unset.
        let deps = mk_deps(Arc::new(ApproveGate), voice, ModelFamily::Default).await;
        let err = preflight(
            &deps,
            &json!({"number": "+14155551234", "objective": "test"}),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("missing"),
            "expected 'missing' in error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn place_call_user_party_substitutes_user_number() {
        let deps = mk_deps(
            Arc::new(ApproveGate),
            populated_voice(),
            ModelFamily::Default,
        )
        .await;
        let req = preflight(
            &deps,
            &json!({
                "number": "+15555550000",  // should be IGNORED for user party
                "objective": "reminder",
                "called_party": "user"
            }),
        )
        .unwrap();
        assert_eq!(req.to_number, "+14155550200");
        assert_eq!(req.called_party, CalledParty::User);
    }

    #[tokio::test]
    async fn place_call_user_party_rejects_when_user_number_missing() {
        let mut voice = populated_voice();
        voice.user_number = None;
        let deps = mk_deps(Arc::new(ApproveGate), voice, ModelFamily::Default).await;
        let err = preflight(
            &deps,
            &json!({
                "number": "+15555550000",
                "objective": "reminder",
                "called_party": "user"
            }),
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("user_number"), "got: {msg}");
    }

    #[tokio::test]
    async fn place_call_clamps_max_duration() {
        let deps = mk_deps(
            Arc::new(ApproveGate),
            populated_voice(),
            ModelFamily::Default,
        )
        .await;
        let req = preflight(
            &deps,
            &json!({
                "number": "+14155551234",
                "objective": "x",
                "max_duration_s": 999_999
            }),
        )
        .unwrap();
        assert_eq!(req.max_duration_s, VoiceConfig::HARD_MAX_DURATION_S);
    }

    #[tokio::test]
    async fn place_call_supports_anthropic_family() {
        let deps = mk_deps(
            Arc::new(ApproveGate),
            populated_voice(),
            ModelFamily::ClaudeOpus47,
        )
        .await;
        let req = preflight(&deps, &json!({"number": "+14155551234", "objective": "x"})).unwrap();
        assert!(req.llm_label.contains("deepseek-chat"));
    }

    #[test]
    fn pipecat_kind_maps_families_correctly() {
        // Family label has "claude" in it → Anthropic adapter.
        assert_eq!(
            pipecat_kind_for("anything", ModelFamily::ClaudeOpus47),
            Some(PipecatLlmKind::Anthropic)
        );
        assert_eq!(
            pipecat_kind_for("anything", ModelFamily::ClaudeHaiku45),
            Some(PipecatLlmKind::Anthropic)
        );
        // Gemini → Google.
        assert_eq!(
            pipecat_kind_for("anything", ModelFamily::Gemini3Flash),
            Some(PipecatLlmKind::Google)
        );
        // DeepSeek / GPT / Qwen / Kimi → openai_compat (any non-claude,
        // non-gemini family).
        assert_eq!(
            pipecat_kind_for("anything", ModelFamily::DeepSeekV4Chat),
            Some(PipecatLlmKind::OpenAiCompat)
        );
        assert_eq!(
            pipecat_kind_for("anything", ModelFamily::Gpt5),
            Some(PipecatLlmKind::OpenAiCompat)
        );
        // Default family + id heuristic.
        assert_eq!(
            pipecat_kind_for("openai", ModelFamily::Default),
            Some(PipecatLlmKind::OpenAiCompat)
        );
        assert_eq!(
            pipecat_kind_for("deepseek", ModelFamily::Default),
            Some(PipecatLlmKind::OpenAiCompat)
        );
        assert_eq!(
            pipecat_kind_for("anthropic_byo", ModelFamily::Default),
            Some(PipecatLlmKind::Anthropic)
        );
        assert_eq!(
            pipecat_kind_for("my_gemini_proxy", ModelFamily::Default),
            Some(PipecatLlmKind::Google)
        );
    }
}
