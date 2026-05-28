//! Render the user's enabled HTTP endpoints into a markdown block that the
//! executor pins inside the system prompt's static prefix. Cached between
//! turns; only changes when the user adds/removes/edits an endpoint, so
//! the LCP-cacheable prefix stays stable for normal use.
//!
//! Why this lives in the prompt rather than discovered on demand: real
//! failure observed — agent succeeded on turn 1 (used `http_request →
//! ElevenLabs`), then on turn 2 forgot the endpoint existed and burned 11
//! shell calls trying to install the elevenlabs Python SDK before giving
//! up. Same class of bug as the Whisper rabbit hole. Pinning endpoint
//! metadata in the static prefix keeps known capabilities in sight.
//!
//! The host (athen-app) reads SQLite + matches each endpoint against its
//! preset library to derive a one-line blurb. The executor only frames
//! the resulting string — it never reads endpoint state directly, so the
//! `athen-agent` crate stays free of any persistence dependency.

use std::sync::Arc;

use athen_core::http_endpoint::{AuthMethod, RegisteredEndpoint};
use athen_core::traits::http_endpoint::HttpEndpointStore;
use athen_persistence::http_endpoints::SqliteHttpEndpointStore;

use crate::http_presets;

/// Maximum characters of free-text description per endpoint. Endpoints
/// shouldn't crowd the system prefix with multi-line notes — the
/// per-endpoint detail file is the place for that.
const BLURB_MAX_CHARS: usize = 100;

/// Render the registered-HTTP-endpoints block for the active install.
/// Returns `None` when the store is unwired or no endpoints are enabled
/// (so the executor's `build_endpoints_section` emits zero bytes — the
/// prompt is byte-identical to today's for users who haven't registered
/// anything).
///
/// Errors from the store are logged and swallowed: endpoint listing is
/// enrichment, not a hard requirement, and a SQLite hiccup must not
/// block dispatch.
pub async fn render_endpoints_block(
    store: Option<&Arc<SqliteHttpEndpointStore>>,
) -> Option<String> {
    let store = store?;
    let endpoints = match store.list().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("endpoints_render: list failed: {e}");
            return None;
        }
    };
    let enabled: Vec<RegisteredEndpoint> = endpoints.into_iter().filter(|e| e.enabled).collect();
    if enabled.is_empty() {
        return None;
    }

    let presets = http_presets::presets();

    let mut out = String::new();
    for ep in &enabled {
        let blurb = derive_blurb(ep, &presets);
        let auth = auth_label(&ep.auth_method);
        let auth_suffix = auth.map(|s| format!(" Auth: {s}.")).unwrap_or_default();
        out.push_str(&format!(
            "- **{}** ({}) — {}{}\n",
            ep.name, ep.base_url, blurb, auth_suffix,
        ));
    }
    let trimmed = out.trim_end();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Pick the most informative one-liner for an endpoint, preferring the
/// user's own short notes, then a preset's first sentence, then a
/// generic fallback.
fn derive_blurb(ep: &RegisteredEndpoint, presets: &[http_presets::EndpointPreset]) -> String {
    if let Some(notes) = ep.notes.as_ref() {
        let n = notes.trim();
        if !n.is_empty() && n.chars().count() <= BLURB_MAX_CHARS && !n.contains('\n') {
            return n.to_string();
        }
    }
    if let Some(preset) = match_preset(ep, presets) {
        if let Some(first) = first_sentence(preset.usage_hints) {
            return clamp_chars(&strip_markdown(&first), BLURB_MAX_CHARS);
        }
    }
    "external HTTP endpoint.".to_string()
}

/// Match an endpoint to a preset by `base_url` first (strongest signal —
/// preset URLs are the prefilled value), then by case-insensitive
/// `provider` label as a fallback for endpoints whose URL the user has
/// edited.
fn match_preset<'a>(
    ep: &RegisteredEndpoint,
    presets: &'a [http_presets::EndpointPreset],
) -> Option<&'a http_presets::EndpointPreset> {
    if let Some(p) = presets
        .iter()
        .find(|p| p.base_url.eq_ignore_ascii_case(&ep.base_url))
    {
        return Some(p);
    }
    if !ep.provider.is_empty() {
        if let Some(p) = presets
            .iter()
            .find(|p| p.provider.eq_ignore_ascii_case(&ep.provider))
        {
            return Some(p);
        }
    }
    None
}

/// First sentence of a usage_hints body. Splits on `. ` (period + space)
/// — preset hints are written in prose, so this captures the lede
/// without false-positive splits on `e.g.` or version numbers (no
/// trailing space). Returns `None` only if the input is empty.
fn first_sentence(hints: &str) -> Option<String> {
    let s = hints.trim();
    if s.is_empty() {
        return None;
    }
    let head = match s.split_once(". ") {
        Some((before, _)) => before,
        None => s.split('\n').next().unwrap_or(s),
    };
    let mut head = head.trim().to_string();
    if !head.ends_with('.') {
        head.push('.');
    }
    Some(head)
}

/// Strip the markdown bits the model doesn't need to read inline:
/// `**bold**`, `` `code` ``, leading list markers. Cheap and lossy on
/// purpose — these are one-line summaries, not the full doc.
fn strip_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' {
            // Drop both '*' and '**' runs.
            while matches!(chars.peek(), Some('*')) {
                chars.next();
            }
            continue;
        }
        if c == '`' {
            continue;
        }
        out.push(c);
    }
    out.trim().to_string()
}

/// Clamp by character count, appending `…` when truncation actually
/// happens. Operates on chars (not bytes) so multi-byte UTF-8 doesn't
/// corrupt the cut.
fn clamp_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Short label for the auth method, suitable for inline prose. Returns
/// `None` for `AuthMethod::None` so the renderer drops the "Auth:"
/// fragment entirely on public endpoints.
fn auth_label(auth: &AuthMethod) -> Option<&'static str> {
    match auth {
        AuthMethod::None => None,
        AuthMethod::BearerToken => Some("bearer token"),
        AuthMethod::Header { .. } => Some("api key header"),
        AuthMethod::HeaderPrefixed { .. } => Some("api key header (prefixed)"),
        AuthMethod::QueryParam { .. } => Some("api key query param"),
        AuthMethod::BasicAuth { .. } => Some("basic auth"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use athen_persistence::Database;
    use chrono::Utc;
    use uuid::Uuid;

    async fn fresh_store() -> Arc<SqliteHttpEndpointStore> {
        let db = Database::in_memory().await.unwrap();
        Arc::new(db.http_endpoint_store())
    }

    fn mk_endpoint(name: &str, provider: &str, base_url: &str) -> RegisteredEndpoint {
        RegisteredEndpoint {
            id: Uuid::new_v4(),
            name: name.into(),
            provider: provider.into(),
            base_url: base_url.into(),
            enabled: true,
            auth_method: AuthMethod::BearerToken,
            default_headers: vec![],
            default_query_params: vec![],
            rate_limit: None,
            risk_override: None,
            notes: None,
            last_used: None,
            call_count_30d: 0,
            created_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn unwired_store_returns_none() {
        assert!(render_endpoints_block(None).await.is_none());
    }

    #[tokio::test]
    async fn empty_store_returns_none() {
        let store = fresh_store().await;
        assert!(render_endpoints_block(Some(&store)).await.is_none());
    }

    #[tokio::test]
    async fn disabled_endpoints_excluded() {
        let store = fresh_store().await;
        let mut ep = mk_endpoint("Acme", "Acme", "https://acme.example/");
        ep.enabled = false;
        store.upsert(&ep).await.unwrap();
        assert!(render_endpoints_block(Some(&store)).await.is_none());
    }

    #[tokio::test]
    async fn renders_name_url_and_auth_label() {
        let store = fresh_store().await;
        store
            .upsert(&mk_endpoint("Custom", "Acme", "https://acme.example/v1/"))
            .await
            .unwrap();
        let out = render_endpoints_block(Some(&store)).await.unwrap();
        assert!(out.contains("**Custom**"));
        assert!(out.contains("https://acme.example/v1/"));
        assert!(out.contains("Auth: bearer token."));
    }

    #[tokio::test]
    async fn matches_preset_by_base_url_for_blurb() {
        let store = fresh_store().await;
        // Use the canonical ElevenLabs preset URL so the matcher fires.
        let mut ep = mk_endpoint("ElevenLabs", "ElevenLabs", "https://api.elevenlabs.io/v1/");
        ep.auth_method = AuthMethod::Header {
            name: "xi-api-key".into(),
        };
        store.upsert(&ep).await.unwrap();
        let out = render_endpoints_block(Some(&store)).await.unwrap();
        // First sentence of the preset hints mentions the TTS endpoint.
        assert!(out.contains("text-to-speech"));
        // Header auth is rendered as "api key header".
        assert!(out.contains("Auth: api key header."));
    }

    #[tokio::test]
    async fn user_notes_override_preset_blurb() {
        let store = fresh_store().await;
        let mut ep = mk_endpoint("ElevenLabs", "ElevenLabs", "https://api.elevenlabs.io/v1/");
        ep.notes = Some("Use voice_id 21m00Tcm4TlvDq8ikWAM by default.".into());
        store.upsert(&ep).await.unwrap();
        let out = render_endpoints_block(Some(&store)).await.unwrap();
        assert!(out.contains("Use voice_id 21m00Tcm4TlvDq8ikWAM by default."));
        // Preset blurb must NOT also be glued in.
        assert!(!out.contains("text-to-speech"));
    }

    #[tokio::test]
    async fn long_notes_fall_back_to_preset() {
        let store = fresh_store().await;
        let mut ep = mk_endpoint("ElevenLabs", "ElevenLabs", "https://api.elevenlabs.io/v1/");
        ep.notes = Some("a".repeat(BLURB_MAX_CHARS + 50));
        store.upsert(&ep).await.unwrap();
        let out = render_endpoints_block(Some(&store)).await.unwrap();
        // Notes too long → preset blurb wins.
        assert!(out.contains("text-to-speech"));
    }

    #[tokio::test]
    async fn unknown_endpoint_uses_generic_fallback() {
        let store = fresh_store().await;
        store
            .upsert(&mk_endpoint(
                "Mystery",
                "MysteryCorp",
                "https://mystery.example/v3/",
            ))
            .await
            .unwrap();
        let out = render_endpoints_block(Some(&store)).await.unwrap();
        assert!(out.contains("external HTTP endpoint."));
    }

    #[tokio::test]
    async fn auth_none_omits_auth_fragment() {
        let store = fresh_store().await;
        let mut ep = mk_endpoint("Public", "Public", "https://public.example/");
        ep.auth_method = AuthMethod::None;
        store.upsert(&ep).await.unwrap();
        let out = render_endpoints_block(Some(&store)).await.unwrap();
        assert!(!out.contains("Auth:"));
    }

    #[test]
    fn first_sentence_handles_no_trailing_period() {
        assert_eq!(first_sentence("hello world"), Some("hello world.".into()));
        assert_eq!(first_sentence("a. b"), Some("a.".into()));
        assert_eq!(first_sentence(""), None);
    }

    #[test]
    fn clamp_chars_appends_ellipsis_only_when_cut() {
        assert_eq!(clamp_chars("short", 100), "short");
        let cut = clamp_chars("aaaaaaaaaa", 5);
        assert_eq!(cut.chars().count(), 5);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn strip_markdown_removes_stars_and_backticks() {
        assert_eq!(strip_markdown("**bold** and `code`"), "bold and code");
    }
}
