//! Static error catalog for IMAP / SMTP failures.
//!
//! Phase 1 of the email setup wizard. Tier 1 of the two-tier translator
//! described in `docs/EMAIL_SETUP.md`: ~13 well-known error patterns that
//! every IMAP/SMTP user hits get hand-written, actionable copy. Unknown
//! errors return `None` — Phase 3 hooks an LLM call into the same shape.
//!
//! All matching is case-insensitive substring; the goal is "does this
//! raw error string mention `AUTHENTICATIONFAILED`?", not parsing.

use serde::{Deserialize, Serialize};

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
}
