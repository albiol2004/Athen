//! Email provider hint types — shared between the autodetect pipeline and
//! the Settings → Email UI.
//!
//! Phase 1 of the email setup wizard (docs/EMAIL_SETUP.md). The autodetect
//! chain produces a [`ProviderHint`], the UI consumes it to pre-fill the
//! IMAP/SMTP form and surface provider-specific guidance (app-password URLs,
//! Bridge requirements, etc.). All types serde-derived so they cross the
//! Tauri command boundary directly.
//!
//! The "source" field on a [`ProviderHint`] lets the UI tell the user *how*
//! the settings were resolved — a hardcoded match is always trustworthy, a
//! Thunderbird autoconfig hit is only as trustworthy as the autoconfig
//! server, and an MX recheck / hostname probe (Phase 4) is best-effort.

use serde::{Deserialize, Serialize};

/// Transport security for an IMAP/SMTP connection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    /// Implicit TLS from the first byte — port 993 (IMAP) / 465 (SMTP).
    Ssl,
    /// Plaintext start, then upgrade via STARTTLS — port 143 (IMAP) /
    /// 587 (SMTP).
    StartTls,
    /// Cleartext for the life of the connection. Never recommended; only
    /// emitted by autoconfig for legacy / self-hosted servers.
    None,
}

/// A single server endpoint — host + port + transport security.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerHint {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

/// How the provider authenticates users for IMAP/SMTP.
///
/// Drives the UX: `AppPassword` shows the deep-link button, `OAuth2` will
/// route to the device-code flow (Move #3, not Phase 1), `BridgeRequired`
/// gates the form behind a "Bridge running?" check.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// Plain account password (rare nowadays — GMX is the only mainstream
    /// example). Most providers gate IMAP/SMTP behind a per-app password.
    Password,
    /// Provider-issued app-specific password (Gmail, iCloud, Yahoo, etc.).
    /// `ProviderHint::app_password_url` points at the page where the user
    /// generates one.
    AppPassword,
    /// XOAUTH2 — not implemented in Phase 1, reserved for Move #3 in the
    /// integrations push.
    OAuth2,
    /// Provider requires running a local bridge (Proton Mail Bridge). The
    /// settings point at `127.0.0.1`; the UI should gate the form on a
    /// reachability probe.
    BridgeRequired,
}

/// Where the autodetect chain found this hint. Drives UI trust messaging
/// ("matched Gmail" vs "found via Thunderbird ISP database").
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderSource {
    /// Matched against the in-code provider table. Highest confidence.
    Hardcoded,
    /// Returned by Thunderbird-style autoconfig XML (any of the three URLs
    /// in the detection chain).
    ThunderbirdAutoconfig,
    /// Apex domain missed; matched after looking up the MX record. Phase 4.
    MxRecheck,
    /// Last-resort TCP probe of `imap.<domain>:993`, etc. Phase 4.
    HostnameProbe,
    /// User-supplied via the Advanced disclosure. The UI tags hints it
    /// receives back from the form with this so a re-test doesn't lose
    /// provenance.
    Manual,
}

/// Best-guess server settings for a user-supplied email address.
///
/// Returned by the autodetect chain. The Settings → Email panel pre-fills
/// the form from this struct, shows the `notes` line under the password
/// field, and renders the app-password deep-link button when
/// `app_password_url` is `Some`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderHint {
    /// Human-readable name shown in the success banner — "Connected to
    /// Gmail as alice@gmail.com".
    pub display_name: String,
    /// IMAP server settings.
    pub incoming: ServerHint,
    /// SMTP server settings.
    pub outgoing: ServerHint,
    /// Authentication mechanism the user needs to supply credentials for.
    pub auth_kind: AuthKind,
    /// Deep-link to the provider's app-password generation page. `None`
    /// for OAuth2 / plain-password providers.
    pub app_password_url: Option<String>,
    /// Extra guidance shown under the password field — "Gmail needs 2FA
    /// enabled first", "Proton requires Bridge running at 127.0.0.1:1143",
    /// etc.
    pub notes: Option<String>,
    /// How this hint was resolved. Drives UI provenance messaging.
    pub source: ProviderSource,
}
