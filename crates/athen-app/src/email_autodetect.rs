//! Email provider autodetection.
//!
//! Phase 1 of the email setup wizard (docs/EMAIL_SETUP.md). Two-stage
//! chain: hardcoded provider table → Thunderbird-style autoconfig fetcher.
//! MX recheck and hostname probing are Phase 4.
//!
//! Stays free of any cross-crate dependencies beyond `athen-core` types.
//! The fetcher uses `reqwest` (rustls-tls only, per workspace convention)
//! and `quick-xml` for the autoconfig XML; both lookups have a 5 s
//! timeout and run in parallel via `futures::future::select_ok` so the
//! first hit wins.

use std::time::Duration;

use athen_core::email_provider::{AuthKind, ProviderHint, ProviderSource, Security, ServerHint};
use futures::future::FutureExt;
use quick_xml::events::Event;
use quick_xml::Reader;

/// Top-level entry point. Returns the first hit from the detection chain,
/// or `None` if nothing matched.
pub async fn detect(email: &str) -> Option<ProviderHint> {
    if let Some(hit) = detect_hardcoded(email) {
        return Some(hit);
    }
    detect_thunderbird(email).await.ok().flatten()
}

/// Extract the lowercased domain from an email address. Returns `None`
/// for inputs without exactly one `@`.
fn domain_of(email: &str) -> Option<String> {
    let trimmed = email.trim();
    let at = trimmed.find('@')?;
    let domain = trimmed[at + 1..].trim();
    if domain.is_empty() || domain.contains('@') {
        return None;
    }
    Some(domain.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Hardcoded provider table
// ---------------------------------------------------------------------------

/// Match the email domain against the in-code provider table. Values
/// match `docs/EMAIL_SETUP.md` exactly — the doc is the spec, the code is
/// the source of truth.
pub fn detect_hardcoded(email: &str) -> Option<ProviderHint> {
    let domain = domain_of(email)?;

    // Match in declaration order; first hit wins. Each branch returns a
    // fully-populated `ProviderHint`. Values audited 2026-05-13 from the
    // wizard design doc.
    if matches!(domain.as_str(), "gmail.com" | "googlemail.com") {
        return Some(ProviderHint {
            display_name: "Gmail".into(),
            incoming: server("imap.gmail.com", 993, Security::Ssl),
            outgoing: server("smtp.gmail.com", 587, Security::StartTls),
            auth_kind: AuthKind::AppPassword,
            app_password_url: Some("https://myaccount.google.com/apppasswords".into()),
            notes: Some(
                "Gmail requires 2-Step Verification to be enabled before app passwords can be created.".into(),
            ),
            source: ProviderSource::Hardcoded,
        });
    }

    if matches!(
        domain.as_str(),
        "outlook.com" | "hotmail.com" | "live.com" | "msn.com"
    ) {
        return Some(ProviderHint {
            display_name: "Outlook.com".into(),
            incoming: server("outlook.office365.com", 993, Security::Ssl),
            outgoing: server("smtp.office365.com", 587, Security::StartTls),
            auth_kind: AuthKind::AppPassword,
            app_password_url: Some("https://account.live.com/proofs/AppPassword".into()),
            notes: Some(
                "Outlook is phasing out app passwords in 2026 — OAuth sign-in is coming in a future update.".into(),
            ),
            source: ProviderSource::Hardcoded,
        });
    }

    if matches!(domain.as_str(), "icloud.com" | "me.com" | "mac.com") {
        return Some(ProviderHint {
            display_name: "iCloud Mail".into(),
            incoming: server("imap.mail.me.com", 993, Security::Ssl),
            outgoing: server("smtp.mail.me.com", 587, Security::StartTls),
            auth_kind: AuthKind::AppPassword,
            app_password_url: Some("https://appleid.apple.com".into()),
            notes: Some(
                "Generate an app-specific password under \"Sign-In and Security\" at appleid.apple.com (2FA required).".into(),
            ),
            source: ProviderSource::Hardcoded,
        });
    }

    if matches!(domain.as_str(), "fastmail.com" | "fastmail.fm") {
        return Some(ProviderHint {
            display_name: "Fastmail".into(),
            incoming: server("imap.fastmail.com", 993, Security::Ssl),
            outgoing: server("smtp.fastmail.com", 465, Security::Ssl),
            auth_kind: AuthKind::AppPassword,
            app_password_url: Some(
                "https://www.fastmail.com/settings/security/apppasswords".into(),
            ),
            notes: None,
            source: ProviderSource::Hardcoded,
        });
    }

    if matches!(
        domain.as_str(),
        "yahoo.com" | "yahoo.co.uk" | "ymail.com" | "rocketmail.com"
    ) {
        return Some(ProviderHint {
            display_name: "Yahoo Mail".into(),
            incoming: server("imap.mail.yahoo.com", 993, Security::Ssl),
            outgoing: server("smtp.mail.yahoo.com", 465, Security::Ssl),
            auth_kind: AuthKind::AppPassword,
            app_password_url: Some("https://login.yahoo.com/account/security/app-passwords".into()),
            notes: Some(
                "Yahoo requires 2-Step Verification before app passwords can be generated.".into(),
            ),
            source: ProviderSource::Hardcoded,
        });
    }

    if matches!(domain.as_str(), "proton.me" | "protonmail.com" | "pm.me") {
        return Some(ProviderHint {
            display_name: "Proton Mail".into(),
            incoming: server("127.0.0.1", 1143, Security::StartTls),
            outgoing: server("127.0.0.1", 1025, Security::StartTls),
            auth_kind: AuthKind::BridgeRequired,
            app_password_url: Some("https://proton.me/mail/bridge".into()),
            notes: Some(
                "Proton Mail needs Proton Bridge running locally (paid plan required). Install Bridge and use the credentials it generates.".into(),
            ),
            source: ProviderSource::Hardcoded,
        });
    }

    if matches!(domain.as_str(), "yandex.com" | "yandex.ru") {
        return Some(ProviderHint {
            display_name: "Yandex Mail".into(),
            incoming: server("imap.yandex.com", 993, Security::Ssl),
            outgoing: server("smtp.yandex.com", 465, Security::Ssl),
            auth_kind: AuthKind::AppPassword,
            app_password_url: Some("https://id.yandex.com/security/app-passwords".into()),
            notes: None,
            source: ProviderSource::Hardcoded,
        });
    }

    if matches!(domain.as_str(), "gmx.com" | "gmx.net" | "gmx.de") {
        return Some(ProviderHint {
            display_name: "GMX Mail".into(),
            incoming: server("imap.gmx.com", 993, Security::Ssl),
            outgoing: server("mail.gmx.com", 587, Security::StartTls),
            auth_kind: AuthKind::Password,
            app_password_url: Some("https://www.gmx.com/mail/settings/".into()),
            notes: Some("Enable IMAP/POP access in GMX webmail settings before connecting.".into()),
            source: ProviderSource::Hardcoded,
        });
    }

    if matches!(domain.as_str(), "zoho.com" | "zoho.eu") {
        return Some(ProviderHint {
            display_name: "Zoho Mail".into(),
            incoming: server("imap.zoho.com", 993, Security::Ssl),
            outgoing: server("smtp.zoho.com", 465, Security::Ssl),
            auth_kind: AuthKind::AppPassword,
            app_password_url: Some("https://accounts.zoho.com/home#security/apppasswords".into()),
            notes: Some(
                "Free Zoho accounts require an app-specific password for IMAP/SMTP.".into(),
            ),
            source: ProviderSource::Hardcoded,
        });
    }

    if domain == "aol.com" {
        return Some(ProviderHint {
            display_name: "AOL Mail".into(),
            incoming: server("imap.aol.com", 993, Security::Ssl),
            outgoing: server("smtp.aol.com", 465, Security::Ssl),
            auth_kind: AuthKind::AppPassword,
            app_password_url: Some("https://login.aol.com/account/security/app-passwords".into()),
            notes: Some(
                "AOL requires 2-Step Verification before app passwords can be generated.".into(),
            ),
            source: ProviderSource::Hardcoded,
        });
    }

    None
}

fn server(host: &str, port: u16, security: Security) -> ServerHint {
    ServerHint {
        host: host.to_string(),
        port,
        security,
    }
}

// ---------------------------------------------------------------------------
// Thunderbird autoconfig
// ---------------------------------------------------------------------------

/// Try the three Thunderbird-style autoconfig endpoints in parallel and
/// return the first successful parse. Returns `Ok(None)` if every URL
/// resolved but none produced a usable IMAP+SMTP pair; returns `Err`
/// only on bad input (no domain).
pub async fn detect_thunderbird(email: &str) -> Result<Option<ProviderHint>, String> {
    let domain = domain_of(email).ok_or_else(|| "invalid email address".to_string())?;
    let encoded = urlencode(email.trim());

    let urls = [
        format!("https://autoconfig.{domain}/mail/config-v1.1.xml?emailaddress={encoded}"),
        format!(
            "https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress={encoded}"
        ),
        format!("https://autoconfig.thunderbird.net/v1.1/{domain}"),
    ];

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => return Err(format!("http client build: {e}")),
    };

    // Spawn one future per URL; first that yields a parsed hint wins via
    // `select_ok`. A 404 / parse-error short-circuits to `Err` for that
    // future and `select_ok` moves on. If all three error out we return
    // `Ok(None)` so the caller falls back to the manual path.
    let futures: Vec<_> = urls
        .iter()
        .map(|u| fetch_and_parse(client.clone(), u.clone()).boxed())
        .collect();

    match futures::future::select_ok(futures).await {
        Ok((hint, _)) => Ok(Some(hint)),
        Err(_) => Ok(None),
    }
}

async fn fetch_and_parse(client: reqwest::Client, url: String) -> Result<ProviderHint, String> {
    let res = client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("GET {url}: HTTP {}", res.status()));
    }
    let body = res
        .text()
        .await
        .map_err(|e| format!("read body {url}: {e}"))?;
    parse_autoconfig_xml(&body)
        .ok_or_else(|| format!("no usable IMAP+SMTP pair in autoconfig from {url}"))
}

/// Parse a Thunderbird `clientConfig` document. Returns the first
/// `(imap, smtp)` pair found, or `None` if either side is missing.
///
/// We use a streaming `quick-xml` reader because:
/// 1. Most autoconfig docs are tiny (< 4 KB) — DOM overhead is silly.
/// 2. We only care about a fixed set of elements; mis-formed input just
///    leaves fields blank and we discard the document.
pub fn parse_autoconfig_xml(body: &str) -> Option<ProviderHint> {
    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();

    let mut display_name: Option<String> = None;

    let mut current_section: Section = Section::None;
    let mut current_field: Field = Field::None;

    let mut imap_host: Option<String> = None;
    let mut imap_port: Option<u16> = None;
    let mut imap_security: Option<Security> = None;

    let mut smtp_host: Option<String> = None;
    let mut smtp_port: Option<u16> = None;
    let mut smtp_security: Option<Security> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.name();
                let local = std::str::from_utf8(name.as_ref()).unwrap_or("");
                match local {
                    "incomingServer" => {
                        // `type="imap"` → eligible. Everything else (pop3,
                        // exchange, etc.) is skipped.
                        let is_imap = e
                            .attributes()
                            .flatten()
                            .any(|a| a.key.as_ref() == b"type" && a.value.as_ref() == b"imap");
                        current_section = if is_imap {
                            Section::Imap
                        } else {
                            Section::Skip
                        };
                    }
                    "outgoingServer" => {
                        let is_smtp = e
                            .attributes()
                            .flatten()
                            .any(|a| a.key.as_ref() == b"type" && a.value.as_ref() == b"smtp");
                        current_section = if is_smtp {
                            Section::Smtp
                        } else {
                            Section::Skip
                        };
                    }
                    "hostname" => current_field = Field::Hostname,
                    "port" => current_field = Field::Port,
                    "socketType" => current_field = Field::SocketType,
                    "displayName" => current_field = Field::DisplayName,
                    _ => current_field = Field::None,
                }
            }
            Ok(Event::Text(t)) => {
                let text = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                // First-write-wins for every field: autoconfig docs
                // sometimes list alternate hostnames; we take the first
                // and ignore the rest rather than overwrite.
                match (current_section, current_field) {
                    (_, Field::DisplayName) if display_name.is_none() && !text.is_empty() => {
                        display_name = Some(text);
                    }
                    (Section::Imap, Field::Hostname) if imap_host.is_none() => {
                        imap_host = Some(text);
                    }
                    (Section::Imap, Field::Port) if imap_port.is_none() => {
                        imap_port = text.parse().ok();
                    }
                    (Section::Imap, Field::SocketType) if imap_security.is_none() => {
                        imap_security = parse_socket_type(&text);
                    }
                    (Section::Smtp, Field::Hostname) if smtp_host.is_none() => {
                        smtp_host = Some(text);
                    }
                    (Section::Smtp, Field::Port) if smtp_port.is_none() => {
                        smtp_port = text.parse().ok();
                    }
                    (Section::Smtp, Field::SocketType) if smtp_security.is_none() => {
                        smtp_security = parse_socket_type(&text);
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let local = std::str::from_utf8(name.as_ref()).unwrap_or("");
                match local {
                    "incomingServer" | "outgoingServer" => current_section = Section::None,
                    "hostname" | "port" | "socketType" | "displayName" => {
                        current_field = Field::None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
        buf.clear();
    }

    let incoming = ServerHint {
        host: imap_host?,
        port: imap_port?,
        security: imap_security.unwrap_or(Security::Ssl),
    };
    let outgoing = ServerHint {
        host: smtp_host?,
        port: smtp_port?,
        security: smtp_security.unwrap_or(Security::StartTls),
    };

    Some(ProviderHint {
        display_name: display_name.unwrap_or_else(|| "Email".to_string()),
        incoming,
        outgoing,
        // Autoconfig doesn't tell us whether the password is "app" or
        // "account"; we default to plain Password and let the error
        // translator nudge users toward app passwords when AUTH fails.
        auth_kind: AuthKind::Password,
        app_password_url: None,
        notes: None,
        source: ProviderSource::ThunderbirdAutoconfig,
    })
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Section {
    None,
    Imap,
    Smtp,
    /// `incomingServer type="pop3"` and friends — we read the elements
    /// but discard them.
    Skip,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Field {
    None,
    Hostname,
    Port,
    SocketType,
    DisplayName,
}

fn parse_socket_type(value: &str) -> Option<Security> {
    match value.trim().to_ascii_uppercase().as_str() {
        "SSL" => Some(Security::Ssl),
        "STARTTLS" => Some(Security::StartTls),
        "PLAIN" | "NONE" => Some(Security::None),
        _ => None,
    }
}

/// Minimal URL-encoder for the email-address query parameter. Avoids
/// pulling in a full `urlencoding` crate for a one-shot call site —
/// only printable ASCII passes through unchanged, everything else is
/// percent-encoded.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let is_unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if is_unreserved {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_hardcoded_gmail() {
        let hint = detect_hardcoded("alice@gmail.com").expect("gmail should match");
        assert_eq!(hint.display_name, "Gmail");
        assert_eq!(hint.incoming.host, "imap.gmail.com");
        assert_eq!(hint.incoming.port, 993);
        assert_eq!(hint.incoming.security, Security::Ssl);
        assert_eq!(hint.outgoing.host, "smtp.gmail.com");
        assert_eq!(hint.outgoing.port, 587);
        assert_eq!(hint.outgoing.security, Security::StartTls);
        assert_eq!(hint.auth_kind, AuthKind::AppPassword);
        assert!(hint.app_password_url.is_some());
        assert_eq!(hint.source, ProviderSource::Hardcoded);
    }

    #[test]
    fn detect_hardcoded_googlemail_alias() {
        assert!(detect_hardcoded("alice@googlemail.com").is_some());
    }

    #[test]
    fn detect_hardcoded_outlook_family() {
        for d in ["outlook.com", "hotmail.com", "live.com", "msn.com"] {
            let hint = detect_hardcoded(&format!("user@{d}"))
                .unwrap_or_else(|| panic!("{d} should match"));
            assert_eq!(hint.incoming.host, "outlook.office365.com");
        }
    }

    #[test]
    fn detect_hardcoded_icloud_family() {
        for d in ["icloud.com", "me.com", "mac.com"] {
            let hint = detect_hardcoded(&format!("user@{d}"))
                .unwrap_or_else(|| panic!("{d} should match"));
            assert_eq!(hint.display_name, "iCloud Mail");
        }
    }

    #[test]
    fn detect_hardcoded_fastmail_family() {
        for d in ["fastmail.com", "fastmail.fm"] {
            assert!(detect_hardcoded(&format!("u@{d}")).is_some());
        }
    }

    #[test]
    fn detect_hardcoded_yahoo_family() {
        for d in ["yahoo.com", "yahoo.co.uk", "ymail.com", "rocketmail.com"] {
            let hint =
                detect_hardcoded(&format!("u@{d}")).unwrap_or_else(|| panic!("{d} should match"));
            assert_eq!(hint.display_name, "Yahoo Mail");
            assert_eq!(hint.outgoing.port, 465);
        }
    }

    #[test]
    fn detect_hardcoded_proton_flags_bridge() {
        for d in ["proton.me", "protonmail.com", "pm.me"] {
            let hint =
                detect_hardcoded(&format!("u@{d}")).unwrap_or_else(|| panic!("{d} should match"));
            assert_eq!(hint.auth_kind, AuthKind::BridgeRequired);
            assert_eq!(hint.incoming.host, "127.0.0.1");
            assert_eq!(hint.incoming.port, 1143);
            assert_eq!(hint.outgoing.port, 1025);
        }
    }

    #[test]
    fn detect_hardcoded_yandex_family() {
        for d in ["yandex.com", "yandex.ru"] {
            assert!(detect_hardcoded(&format!("u@{d}")).is_some());
        }
    }

    #[test]
    fn detect_hardcoded_gmx_family() {
        for d in ["gmx.com", "gmx.net", "gmx.de"] {
            let hint =
                detect_hardcoded(&format!("u@{d}")).unwrap_or_else(|| panic!("{d} should match"));
            assert_eq!(hint.auth_kind, AuthKind::Password);
        }
    }

    #[test]
    fn detect_hardcoded_zoho_family() {
        for d in ["zoho.com", "zoho.eu"] {
            assert!(detect_hardcoded(&format!("u@{d}")).is_some());
        }
    }

    #[test]
    fn detect_hardcoded_aol() {
        assert!(detect_hardcoded("user@aol.com").is_some());
    }

    #[test]
    fn detect_hardcoded_case_insensitive() {
        assert!(detect_hardcoded("Alice@GMAIL.COM").is_some());
    }

    #[test]
    fn detect_hardcoded_unknown_domain_returns_none() {
        assert!(detect_hardcoded("alice@example.com").is_none());
        assert!(detect_hardcoded("alice@mycompany.co.uk").is_none());
    }

    #[test]
    fn detect_hardcoded_malformed_email_returns_none() {
        assert!(detect_hardcoded("not-an-email").is_none());
        assert!(detect_hardcoded("a@b@c").is_none());
        assert!(detect_hardcoded("alice@").is_none());
    }

    #[test]
    fn parse_autoconfig_xml_basic() {
        let xml = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="example.com">
    <displayName>Example Mail</displayName>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <authentication>password-cleartext</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;
        let hint = parse_autoconfig_xml(xml).expect("should parse");
        assert_eq!(hint.display_name, "Example Mail");
        assert_eq!(hint.incoming.host, "imap.example.com");
        assert_eq!(hint.incoming.port, 993);
        assert_eq!(hint.incoming.security, Security::Ssl);
        assert_eq!(hint.outgoing.host, "smtp.example.com");
        assert_eq!(hint.outgoing.port, 587);
        assert_eq!(hint.outgoing.security, Security::StartTls);
        assert_eq!(hint.source, ProviderSource::ThunderbirdAutoconfig);
    }

    #[test]
    fn parse_autoconfig_xml_skips_pop3() {
        let xml = r#"<clientConfig>
  <emailProvider>
    <incomingServer type="pop3">
      <hostname>pop.example.com</hostname>
      <port>995</port>
      <socketType>SSL</socketType>
    </incomingServer>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>465</port>
      <socketType>SSL</socketType>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;
        let hint = parse_autoconfig_xml(xml).expect("should parse imap, not pop3");
        assert_eq!(hint.incoming.host, "imap.example.com");
        assert_eq!(hint.outgoing.security, Security::Ssl);
    }

    #[test]
    fn parse_autoconfig_xml_missing_smtp_returns_none() {
        let xml = r#"<clientConfig><emailProvider>
            <incomingServer type="imap">
              <hostname>imap.example.com</hostname>
              <port>993</port>
              <socketType>SSL</socketType>
            </incomingServer>
        </emailProvider></clientConfig>"#;
        assert!(parse_autoconfig_xml(xml).is_none());
    }

    #[test]
    fn urlencode_basic() {
        assert_eq!(urlencode("alice@example.com"), "alice%40example.com");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("abc-_.~"), "abc-_.~");
    }
}
