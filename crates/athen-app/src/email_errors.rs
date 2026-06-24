//! Static error catalog for IMAP / SMTP failures + LLM fallback.
//!
//! Two-tier translator described in `docs/EMAIL_SETUP.md`:
//! - Tier 1 (`translate`): ~13 well-known error patterns that every
//!   IMAP/SMTP user hits get hand-written, actionable copy. Synchronous,
//!   no I/O, never returns `None` for known shapes.
//! - Tier 2 (`translate_with_llm`): when tier 1 misses, ask the cheap
//!   LLM profile for a one-shot JSON translation. Session-scoped
//!   in-memory cache keyed on `hash(error + "|" + domain)` keeps repeat
//!   lookups free. On LLM failure / timeout / parse failure → `None`,
//!   and the FE falls back to showing the raw error.
//!
//! All tier-1 matching is case-insensitive substring; the goal is
//! "does this raw error string mention `AUTHENTICATIONFAILED`?", not
//! parsing.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// Plain-English version of a raw IMAP/SMTP error.
///
/// Renders as a banner: `title` is the bold first line, `body` is the
/// follow-up sentence, and `action_label`/`action_url` together render
/// an optional contextual button (e.g. "Open Google app passwords").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslatedError {
    pub title: String,
    pub body: String,
    pub action_label: Option<String>,
    pub action_url: Option<String>,
}

impl TranslatedError {
    fn new(title: &str, body: &str) -> Self {
        Self {
            title: title.to_string(),
            body: body.to_string(),
            action_label: None,
            action_url: None,
        }
    }

    fn with_action(mut self, label: &str, url: &str) -> Self {
        self.action_label = Some(label.to_string());
        self.action_url = Some(url.to_string());
        self
    }
}

/// Try to match a raw error against the static catalog. The optional
/// `domain` argument narrows the `AUTHENTICATIONFAILED` family to the
/// right provider's app-password URL.
///
/// Returns `None` if no rule matches — the LLM fallback (Phase 3) is
/// expected to take it from there.
pub fn translate(raw_error: &str, domain: Option<&str>) -> Option<TranslatedError> {
    let lower = raw_error.to_ascii_lowercase();
    let domain = domain.map(|d| d.trim().to_ascii_lowercase());

    // ----- Web-login required (Google's pre-AUTH gate) ---------------
    // Order matters: `imap_alert_web_login_required` is a more specific
    // hint than a bare `AUTHENTICATIONFAILED`. Check it first so it
    // wins when both substrings are present.
    if lower.contains("web login required") || (lower.contains("alert") && lower.contains("login"))
    {
        return Some(
            TranslatedError::new(
                "Google flagged this sign-in",
                "Open the link to verify, then try again. Gmail blocks new IMAP clients until you confirm the sign-in.",
            )
            .with_action(
                "Open Google security checkup",
                "https://myaccount.google.com/security",
            ),
        );
    }

    // ----- IMAP AUTHENTICATIONFAILED, domain-aware -------------------
    if lower.contains("authenticationfailed") {
        if let Some(d) = domain.as_deref() {
            if d.contains("gmail") || d.contains("googlemail") {
                return Some(
                    TranslatedError::new(
                        "Gmail needs an app-specific password",
                        "Make one with the button above, then paste it here. Your normal Gmail password won't work.",
                    )
                    .with_action(
                        "Open Google app passwords",
                        "https://myaccount.google.com/apppasswords",
                    ),
                );
            }
            if d.contains("outlook")
                || d.contains("hotmail")
                || d.contains("live")
                || d.contains("msn")
                || d.contains("office365")
            {
                return Some(
                    TranslatedError::new(
                        "Outlook stopped accepting passwords for this method",
                        "Use the new app-password page, or sign in with browser (coming soon).",
                    )
                    .with_action(
                        "Open Outlook app passwords",
                        "https://account.live.com/proofs/AppPassword",
                    ),
                );
            }
            if d.contains("icloud") || d.contains("me.com") || d.contains("mac.com") {
                return Some(
                    TranslatedError::new(
                        "iCloud needs an app-specific password",
                        "Generate one under Sign-In and Security at appleid.apple.com — your Apple ID password alone won't work here.",
                    )
                    .with_action("Open Apple ID", "https://appleid.apple.com"),
                );
            }
            if d.contains("yahoo") || d.contains("ymail") || d.contains("rocketmail") {
                return Some(
                    TranslatedError::new(
                        "Yahoo needs an app-specific password",
                        "Generate one at the Yahoo security page — your regular password won't be accepted for IMAP.",
                    )
                    .with_action(
                        "Open Yahoo app passwords",
                        "https://login.yahoo.com/account/security/app-passwords",
                    ),
                );
            }
        }
        // Generic fallback for unknown providers.
        return Some(TranslatedError::new(
            "The server rejected your username or password",
            "Many providers (Gmail, iCloud, Yahoo, Outlook) need an app-specific password instead of your account password.",
        ));
    }

    // ----- SMTP 535 family -------------------------------------------
    if lower.contains("535 5.7.139") || lower.contains("535-5.7.139") {
        return Some(
            TranslatedError::new(
                "Outlook requires modern authentication",
                "Microsoft is phasing out basic SMTP passwords. Use an app-specific password (coming soon: sign-in with browser).",
            )
            .with_action(
                "Open Outlook app passwords",
                "https://account.live.com/proofs/AppPassword",
            ),
        );
    }
    if lower.contains("535 5.7.8") || lower.contains("535-5.7.8") || lower.contains("535 ") {
        return Some(TranslatedError::new(
            "The SMTP server rejected your password",
            "Same as IMAP — the password isn't accepted. Did you paste an app password rather than your account password?",
        ));
    }
    if lower.contains("530 5.7.0")
        || lower.contains("530-5.7.0")
        || lower.contains("authentication required")
        || lower.contains("auth required")
    {
        return Some(TranslatedError::new(
            "The SMTP server refused to send without authentication",
            "Add your password under SMTP — outbound mail can't go through this server without it.",
        ));
    }

    // ----- IMAP capacity / quota / connection limits -----------------
    if lower.contains("too many")
        && (lower.contains("connection") || lower.contains("simultaneous"))
    {
        return Some(TranslatedError::new(
            "Too many email clients on this account",
            "Close other email apps connected to this mailbox, wait a minute, then try again.",
        ));
    }
    if lower.contains("over quota")
        || lower.contains("mailbox is full")
        || lower.contains("quota exceeded")
    {
        return Some(TranslatedError::new(
            "Your mailbox is full",
            "Free up space at the provider's web interface, then retry the connection.",
        ));
    }

    // ----- IMAP STARTTLS unsupported (our own surfaced error) --------
    if lower.contains("starttls is not supported") || lower.contains("starttls unsupported") {
        return Some(TranslatedError::new(
            "STARTTLS isn't supported here",
            "Try implicit SSL/TLS on port 993 for IMAP (or 465 for SMTP) instead.",
        ));
    }

    // ----- Transport-layer failures ----------------------------------
    if lower.contains("connection refused") {
        return Some(TranslatedError::new(
            "We can't reach the mail server",
            "The server name or port might be wrong, or a firewall is blocking the connection.",
        ));
    }
    if lower.contains("timed out") || lower.contains("timeout") || lower.contains("did not respond")
    {
        return Some(TranslatedError::new(
            "The server didn't answer in time",
            "Check your internet, the host name, and whether the port is open on your network.",
        ));
    }
    if lower.contains("tls")
        && (lower.contains("handshake")
            || lower.contains("certificate")
            || lower.contains("invalid"))
    {
        return Some(TranslatedError::new(
            "Encryption negotiation failed",
            "If this is a self-hosted server with a custom certificate, you may need manual settings under Advanced.",
        ));
    }

    None
}

// ─── Tier 2: LLM fallback ────────────────────────────────────────────

/// Maximum length of the raw error we feed to the LLM. Some IMAP errors
/// are full stack traces; truncating keeps the prompt sane and avoids
/// burning tokens on noise.
const MAX_RAW_ERROR_LEN: usize = 1500;

/// Reply shape from the LLM. Matches the JSON contract documented in
/// `docs/EMAIL_SETUP.md` ("Error translation" section).
#[derive(Debug, Deserialize)]
struct LlmEmailErrorReply {
    message: String,
    suggestion: String,
    #[serde(default)]
    action_url: Option<String>,
}

impl LlmEmailErrorReply {
    fn into_translated(self) -> TranslatedError {
        let action_label = self
            .action_url
            .as_ref()
            .map(|_| "Open help page".to_string());
        TranslatedError {
            title: self.message,
            body: self.suggestion,
            action_label,
            action_url: self.action_url,
        }
    }
}

/// Session-scoped cache. Module-level rather than on `AppState`
/// because the cache is purely in-memory + transient and nothing
/// outside this module needs to peek at it. `OnceLock` lets us
/// initialise lazily; the `RwLock` is async-friendly.
fn cache() -> &'static RwLock<HashMap<u64, TranslatedError>> {
    static CACHE: OnceLock<RwLock<HashMap<u64, TranslatedError>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Hash key for cache lookup. Uses the stdlib `DefaultHasher`; we
/// don't need cryptographic strength, just a stable bucket.
fn cache_key(raw_error: &str, domain: Option<&str>) -> u64 {
    let mut h = DefaultHasher::new();
    raw_error.hash(&mut h);
    "|".hash(&mut h);
    domain.unwrap_or("").hash(&mut h);
    h.finish()
}

/// System prompt for the LLM fallback. Verbatim from
/// `docs/EMAIL_SETUP.md`.
const LLM_SYSTEM_PROMPT: &str = "You are an email setup assistant. Given a raw IMAP/SMTP error and the email provider domain, return JSON: {message: string, suggestion: string, action_url: string|null}.\n\nRules:\n- One sentence per field. No jargon (no SMTP codes, no RFC numbers, no \"STARTTLS\").\n- Only suggest URLs you are confident exist (google.com support, apple.com support, microsoft.com support).\n- If unsure of the cause, suggest \"Check the server settings under Advanced.\"\n- Never recommend reinstalling, disabling antivirus, or restarting.";

/// Strip leading/trailing whitespace plus ` ```json … ``` ` fences
/// (DeepSeek and Gemini sometimes wrap structured responses). Returns
/// a borrowed slice of `s` so callers can keep ownership.
fn strip_markdown_fence(s: &str) -> &str {
    let trimmed = s.trim();
    // Match leading ```json or ``` and a trailing ```.
    let after_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed);
    let after_open = after_open.trim_start_matches('\n').trim_start();
    after_open
        .strip_suffix("```")
        .map(|s| s.trim_end())
        .unwrap_or(after_open)
        .trim()
}

/// Parse an LLM reply into a `TranslatedError`. Tries plain JSON first,
/// then falls back to fence-stripped JSON. Returns `None` on parse
/// failure.
pub(crate) fn parse_llm_reply(raw: &str) -> Option<TranslatedError> {
    let trimmed = raw.trim();
    if let Ok(reply) = serde_json::from_str::<LlmEmailErrorReply>(trimmed) {
        return Some(reply.into_translated());
    }
    let stripped = strip_markdown_fence(trimmed);
    if stripped == trimmed {
        return None;
    }
    serde_json::from_str::<LlmEmailErrorReply>(stripped)
        .ok()
        .map(LlmEmailErrorReply::into_translated)
}

/// Tier-2 entry point: run the static catalog first; if it misses, fall
/// back to the LLM. Cache results so repeat lookups are free.
///
/// Returns `None` when both tiers strike out — the FE renders the raw
/// error in that case.
///
/// `domain` must be non-empty for the LLM path to fire; without a
/// provider hint the model's suggestions are too generic to be useful.
pub async fn translate_with_llm(
    raw_error: &str,
    domain: Option<&str>,
    router: &dyn athen_core::traits::llm::LlmRouter,
) -> Option<TranslatedError> {
    if let Some(hit) = translate(raw_error, domain) {
        return Some(hit);
    }

    let domain_clean = domain.map(|d| d.trim()).filter(|d| !d.is_empty());
    let domain_clean = domain_clean?;

    let key = cache_key(raw_error, Some(domain_clean));
    {
        let guard = cache().read().await;
        if let Some(hit) = guard.get(&key) {
            tracing::debug!(domain = %domain_clean, "email error translator cache hit");
            return Some(hit.clone());
        }
    }

    // Truncate the raw error so we don't ship a 50KB stack trace.
    let raw_for_prompt: String = if raw_error.chars().count() > MAX_RAW_ERROR_LEN {
        raw_error.chars().take(MAX_RAW_ERROR_LEN).collect()
    } else {
        raw_error.to_string()
    };

    let user_msg = format!(
        "error=\"{}\", domain=\"{}\"",
        raw_for_prompt.replace('"', "\\\""),
        domain_clean.replace('"', "\\\"")
    );

    use athen_core::llm::{
        ChatMessage as LlmChatMessage, LlmRequest, MessageContent as LlmContent, ModelProfile,
        Role as LlmRole,
    };
    let request = LlmRequest {
        profile: ModelProfile::Judges,
        messages: vec![LlmChatMessage {
            role: LlmRole::User,
            content: LlmContent::Text(user_msg),
        }],
        max_tokens: Some(200),
        temperature: Some(0.0),
        tools: None,
        system_prompt: Some(LLM_SYSTEM_PROMPT.to_string()),
        reasoning_effort: athen_core::llm::ReasoningEffort::default(),
    };

    let started = std::time::Instant::now();
    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(60),
        router.route(&request),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(e)) => {
            let preview: String = raw_error.chars().take(200).collect();
            tracing::warn!(error = %e, raw = %preview, "email error LLM translator failed");
            return None;
        }
        Err(_) => {
            let preview: String = raw_error.chars().take(200).collect();
            tracing::warn!(raw = %preview, "email error LLM translator timed out");
            return None;
        }
    };

    let parsed = match parse_llm_reply(&response.content) {
        Some(t) => t,
        None => {
            let preview: String = response.content.chars().take(200).collect();
            tracing::warn!(reply = %preview, "email error LLM reply parse failed");
            return None;
        }
    };

    let elapsed_ms = started.elapsed().as_millis();
    tracing::info!(
        domain = %domain_clean,
        elapsed_ms = elapsed_ms as u64,
        "email error translated via LLM"
    );

    {
        let mut guard = cache().write().await;
        guard.insert(key, parsed.clone());
    }

    Some(parsed)
}

/// Test-only: clear the session cache. Kept under `cfg(test)` so it
/// can't leak into production paths.
#[cfg(test)]
pub(crate) async fn clear_cache_for_tests() {
    cache().write().await.clear();
}

/// Test-only: peek at the cache size.
#[cfg(test)]
pub(crate) async fn cache_size_for_tests() -> usize {
    cache().read().await.len()
}

/// Test-only: directly insert a translated error under the key the
/// real lookup would use. Lets the cache-hit test assert "the LLM
/// is NOT called when the cache is warm" without spinning up a real
/// router.
#[cfg(test)]
pub(crate) async fn prime_cache_for_tests(
    raw_error: &str,
    domain: Option<&str>,
    value: TranslatedError,
) {
    let key = cache_key(raw_error, domain);
    cache().write().await.insert(key, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_gmail_authentication_failed() {
        // Raw form the `imap` crate emits — `[AUTHENTICATIONFAILED]`
        // response code uppercased inside the error display.
        let raw = "Bad Response: [AUTHENTICATIONFAILED] Invalid credentials (Failure)";
        let t = translate(raw, Some("gmail.com")).expect("should translate");
        assert!(t.title.contains("Gmail"));
        assert_eq!(
            t.action_url.as_deref(),
            Some("https://myaccount.google.com/apppasswords")
        );
    }

    #[test]
    fn translates_outlook_authentication_failed() {
        let raw = "AUTHENTICATIONFAILED something something";
        let t = translate(raw, Some("outlook.com")).expect("should translate");
        assert!(t.title.contains("Outlook"));
        assert!(t
            .action_url
            .as_deref()
            .unwrap()
            .contains("account.live.com"));
    }

    #[test]
    fn translates_authentication_failed_with_unknown_domain() {
        let raw = "[AUTHENTICATIONFAILED] no";
        let t = translate(raw, Some("example.com")).expect("should translate");
        assert!(t.title.contains("rejected"));
        assert!(t.action_url.is_none());
    }

    #[test]
    fn translates_smtp_535_outlook_modern_auth() {
        let raw = "535 5.7.139 Authentication unsuccessful, basic authentication is disabled.";
        let t = translate(raw, Some("outlook.com")).expect("should translate");
        assert!(t.title.contains("Outlook"));
    }

    #[test]
    fn translates_smtp_535_generic() {
        let raw = "535 5.7.8 Username and Password not accepted";
        let t = translate(raw, None).expect("should translate");
        assert!(t.title.contains("rejected"));
    }

    #[test]
    fn translates_web_login_required_alert() {
        let raw = "Alert: Web login required: https://accounts.google.com/...";
        let t = translate(raw, Some("gmail.com")).expect("should translate");
        assert!(t.title.contains("flagged"));
    }

    #[test]
    fn translates_connection_refused() {
        let raw = "TCP connect to imap.example.com:993: Connection refused (os error 111)";
        let t = translate(raw, None).expect("should translate");
        assert!(t.title.contains("can't reach"));
    }

    #[test]
    fn translates_tcp_timeout() {
        let raw = "Connection timed out";
        let t = translate(raw, None).expect("should translate");
        assert!(t.title.contains("didn't answer"));
    }

    #[test]
    fn translates_starttls_unsupported() {
        let raw = "IMAP STARTTLS is not supported in this build";
        let t = translate(raw, None).expect("should translate");
        assert!(t.title.contains("STARTTLS"));
    }

    #[test]
    fn translates_too_many_connections() {
        let raw = "[LIMIT] Too many simultaneous connections";
        let t = translate(raw, None).expect("should translate");
        assert!(t.title.contains("Too many"));
    }

    #[test]
    fn translates_over_quota() {
        let raw = "[OVERQUOTA] Mailbox is full";
        let t = translate(raw, None).expect("should translate");
        assert!(t.title.contains("full"));
    }

    #[test]
    fn returns_none_for_unknown_error() {
        assert!(translate("totally unknown gibberish error xyzzy", None).is_none());
        assert!(translate("", None).is_none());
    }

    // ─── Tier-2 (LLM fallback) ───────────────────────────────────────

    /// Serializes the tier-2 tests so the module-level cache + the
    /// recording router can't race. Each test awaits this guard before
    /// touching `clear_cache_for_tests` / `prime_cache_for_tests`.
    /// Uses `tokio::sync::Mutex` so the guard can legally be held
    /// across `.await` points without tripping clippy's
    /// `await_holding_lock` lint.
    fn llm_test_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    #[test]
    fn parse_llm_reply_handles_plain_json() {
        let raw = r#"{"message": "The mail server rejected the login.", "suggestion": "Double-check the username and password.", "action_url": "https://support.example.com"}"#;
        let parsed = parse_llm_reply(raw).expect("plain JSON should parse");
        assert_eq!(parsed.title, "The mail server rejected the login.");
        assert_eq!(parsed.body, "Double-check the username and password.");
        assert_eq!(parsed.action_label.as_deref(), Some("Open help page"));
        assert_eq!(
            parsed.action_url.as_deref(),
            Some("https://support.example.com")
        );
    }

    #[test]
    fn parse_llm_reply_handles_markdown_fenced_json() {
        let raw = "```json\n{\"message\": \"Login refused.\", \"suggestion\": \"Try an app password.\", \"action_url\": null}\n```";
        let parsed = parse_llm_reply(raw).expect("fenced JSON should parse");
        assert_eq!(parsed.title, "Login refused.");
        assert_eq!(parsed.body, "Try an app password.");
        // No URL → no action label.
        assert!(parsed.action_label.is_none());
        assert!(parsed.action_url.is_none());
    }

    #[test]
    fn parse_llm_reply_handles_bare_fenced_json() {
        let raw = "```\n{\"message\":\"x\",\"suggestion\":\"y\"}\n```";
        let parsed = parse_llm_reply(raw).expect("bare-fenced JSON should parse");
        assert_eq!(parsed.title, "x");
        assert_eq!(parsed.body, "y");
        assert!(parsed.action_url.is_none());
    }

    #[test]
    fn parse_llm_reply_rejects_garbage() {
        assert!(parse_llm_reply("not json at all").is_none());
        assert!(parse_llm_reply("").is_none());
        assert!(parse_llm_reply("```json\nnope\n```").is_none());
    }

    /// LlmRouter stub that records how many times it was called and
    /// returns a canned reply.
    struct RecordingRouter {
        replies: std::sync::Mutex<Vec<String>>,
        call_count: std::sync::atomic::AtomicUsize,
    }

    impl RecordingRouter {
        fn new(replies: Vec<&str>) -> Self {
            Self {
                replies: std::sync::Mutex::new(replies.into_iter().map(String::from).collect()),
                call_count: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn calls(&self) -> usize {
            self.call_count.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl athen_core::traits::llm::LlmRouter for RecordingRouter {
        async fn route(
            &self,
            _request: &athen_core::llm::LlmRequest,
        ) -> athen_core::error::Result<athen_core::llm::LlmResponse> {
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut replies = self.replies.lock().unwrap();
            let content = if replies.is_empty() {
                "{\"message\":\"x\",\"suggestion\":\"y\",\"action_url\":null}".to_string()
            } else {
                replies.remove(0)
            };
            Ok(athen_core::llm::LlmResponse {
                content,
                reasoning_content: None,
                model_used: "test-stub".to_string(),
                provider: "test".to_string(),
                usage: athen_core::llm::TokenUsage::default(),
                tool_calls: vec![],
                finish_reason: athen_core::llm::FinishReason::Stop,
            })
        }
        async fn route_streaming(
            &self,
            _request: &athen_core::llm::LlmRequest,
        ) -> athen_core::error::Result<athen_core::llm::LlmStream> {
            unimplemented!("not needed by translator tests")
        }
        async fn budget_remaining(
            &self,
        ) -> athen_core::error::Result<athen_core::llm::BudgetStatus> {
            Ok(athen_core::llm::BudgetStatus {
                daily_limit_usd: None,
                spent_today_usd: 0.0,
                remaining_usd: None,
                tokens_used_today: 0,
            })
        }
    }

    #[tokio::test]
    async fn llm_fallback_short_circuits_on_static_hit() {
        let _g = llm_test_lock().lock().await;
        clear_cache_for_tests().await;
        let router = RecordingRouter::new(vec![]);
        // Static catalog hits → LLM not called.
        let t = translate_with_llm(
            "[AUTHENTICATIONFAILED] bad creds",
            Some("gmail.com"),
            &router,
        )
        .await
        .expect("static catalog should hit");
        assert!(t.title.contains("Gmail"));
        assert_eq!(router.calls(), 0, "LLM must not be called on static hit");
    }

    #[tokio::test]
    async fn llm_fallback_calls_llm_and_caches() {
        let _g = llm_test_lock().lock().await;
        clear_cache_for_tests().await;
        let router = RecordingRouter::new(vec![
            r#"{"message":"Mail host unreachable.","suggestion":"Verify the host name.","action_url":null}"#,
        ]);
        let raw = "weird novel error frob 1234";
        let domain = Some("self-hosted.example.com");

        let first = translate_with_llm(raw, domain, &router)
            .await
            .expect("first call should produce a translation");
        assert_eq!(first.title, "Mail host unreachable.");
        assert_eq!(router.calls(), 1);

        // Second call with same key — cache hit, no extra LLM call.
        let second = translate_with_llm(raw, domain, &router)
            .await
            .expect("second call should hit cache");
        assert_eq!(second, first);
        assert_eq!(
            router.calls(),
            1,
            "cache must short-circuit the second call"
        );
    }

    #[tokio::test]
    async fn llm_fallback_returns_none_without_domain() {
        let _g = llm_test_lock().lock().await;
        clear_cache_for_tests().await;
        let router = RecordingRouter::new(vec![]);
        // No domain → no LLM call (would be too generic to be useful).
        let result = translate_with_llm("totally novel error xyzzy", None, &router).await;
        assert!(result.is_none());
        assert_eq!(router.calls(), 0);

        // Empty/whitespace domain → same.
        let result = translate_with_llm("totally novel error xyzzy", Some("   "), &router).await;
        assert!(result.is_none());
        assert_eq!(router.calls(), 0);
    }

    #[tokio::test]
    async fn llm_fallback_returns_none_on_unparseable_reply() {
        let _g = llm_test_lock().lock().await;
        clear_cache_for_tests().await;
        let initial = cache_size_for_tests().await;
        let router = RecordingRouter::new(vec!["this is not json at all"]);
        let result = translate_with_llm(
            "very unusual postfix error 0xDEADBEEF",
            Some("postfix.example.com"),
            &router,
        )
        .await;
        assert!(result.is_none());
        assert_eq!(router.calls(), 1);
        assert_eq!(
            cache_size_for_tests().await,
            initial,
            "unparseable replies must not populate the cache"
        );
    }

    #[tokio::test]
    async fn llm_fallback_uses_primed_cache_without_calling_llm() {
        let _g = llm_test_lock().lock().await;
        clear_cache_for_tests().await;
        let primed = TranslatedError {
            title: "Cached title".to_string(),
            body: "Cached body".to_string(),
            action_label: None,
            action_url: None,
        };
        let raw = "another novel error frob 4242";
        let domain = Some("primed.example.com");
        prime_cache_for_tests(raw, domain, primed.clone()).await;
        let router = RecordingRouter::new(vec![]);
        let hit = translate_with_llm(raw, domain, &router)
            .await
            .expect("primed cache should produce a hit");
        assert_eq!(hit, primed);
        assert_eq!(router.calls(), 0);
    }
}
