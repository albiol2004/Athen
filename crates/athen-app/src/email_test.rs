//! Test IMAP + SMTP credentials end-to-end, without sending mail.
//!
//! Phase 1 of the email setup wizard (docs/EMAIL_SETUP.md). Reuses the
//! sync `imap` crate (same as `athen-sentidos/src/email.rs`) and the
//! async `lettre` SMTP transport (same as `email_send.rs`). The IMAP
//! half runs in `spawn_blocking` because the crate is sync.
//!
//! Each stage captures *which* stage failed (`tcp` / `tls` / `login` /
//! `list` / `logout` for IMAP — note that `list` is reserved for backward
//! compat in the catalog but is no longer exercised: LOGIN ack is the
//! proof point and some servers reject `LIST "" ""` as "Invalid pattern";
//! `ehlo` / `auth` / `rset` / `quit` for SMTP — `lettre`'s
//! `test_connection()` rolls EHLO+AUTH+RSET+QUIT into a single call, so
//! we report `auth` on its failure since 90% of failures at that step are
//! credential issues). The SMTP half also emits a synthetic
//! `auto_corrected_security` stage on success when the user-supplied
//! Security disagreed with the port and we silently retried with the
//! port-implied choice — the FE uses this to flip the checkbox before
//! persisting.

use std::net::TcpStream;
use std::time::Duration;

use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, Tokio1Executor};
use serde::{Deserialize, Serialize};

use athen_core::email_provider::Security;

/// Per-account connection settings used by `test_connection`. Flat
/// shape so the FE can hand it across the Tauri boundary without a
/// separate marshalling step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmailTestConfig {
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub imap_username: String,

    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    pub smtp_username: String,
}

/// Outcome of a single IMAP or SMTP test pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageResult {
    pub ok: bool,
    /// Raw error string from the underlying crate (`imap::Error` /
    /// `lettre` error chain). The error translator (`email_errors.rs`)
    /// consumes this verbatim — keep it un-pretty-printed so substring
    /// matches in the catalog fire.
    pub error: Option<String>,
    /// Which stage produced the failure. `None` on success.
    /// Values: "tcp", "tls", "login", "list", "logout", "ehlo", "auth",
    /// "rset", "quit". (lettre rolls ehlo→auth→rset→quit into one call;
    /// failures there are reported as "auth".)
    pub stage: Option<String>,
}

impl StageResult {
    fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            stage: None,
        }
    }

    fn fail(stage: &str, error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            stage: Some(stage.to_string()),
        }
    }
}

/// Combined IMAP + SMTP result. Both halves always run — the UI can show
/// one passed and the other failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestResult {
    pub imap: StageResult,
    pub smtp: StageResult,
}

/// Test both IMAP login + SMTP auth using the supplied credentials. Does
/// NOT send an email — SMTP halts after AUTH/RSET/QUIT.
pub async fn test_connection(
    config: &EmailTestConfig,
    password: &str,
    smtp_password: &str,
) -> TestResult {
    let imap = test_imap(config, password).await;
    let smtp = test_smtp(config, smtp_password).await;
    TestResult { imap, smtp }
}

async fn test_imap(config: &EmailTestConfig, password: &str) -> StageResult {
    let host = config.imap_host.clone();
    let port = config.imap_port;
    let security = config.imap_security;
    let username = config.imap_username.clone();
    let password = password.to_string();

    tokio::task::spawn_blocking(move || imap_blocking(&host, port, security, &username, &password))
        .await
        .unwrap_or_else(|e| StageResult::fail("tcp", format!("test task panicked: {e}")))
}

fn imap_blocking(
    host: &str,
    port: u16,
    security: Security,
    username: &str,
    password: &str,
) -> StageResult {
    // Stage 1: TCP connect.
    let tcp = match TcpStream::connect((host, port)) {
        Ok(t) => t,
        Err(e) => return StageResult::fail("tcp", e.to_string()),
    };
    if let Err(e) = tcp.set_read_timeout(Some(Duration::from_secs(5))) {
        return StageResult::fail("tcp", format!("set_read_timeout: {e}"));
    }
    if let Err(e) = tcp.set_write_timeout(Some(Duration::from_secs(5))) {
        return StageResult::fail("tcp", format!("set_write_timeout: {e}"));
    }

    match security {
        Security::Ssl => imap_blocking_tls(host, tcp, username, password),
        Security::StartTls => imap_blocking_starttls(host, tcp, username, password),
        Security::None => imap_blocking_plain(tcp, username, password),
    }
}

fn imap_blocking_tls(host: &str, tcp: TcpStream, username: &str, password: &str) -> StageResult {
    let connector = match rustls_connector::RustlsConnector::new_with_native_certs() {
        Ok(c) => c,
        Err(e) => return StageResult::fail("tls", format!("connector setup: {e}")),
    };
    let tls = match connector.connect(host, tcp) {
        Ok(s) => s,
        Err(e) => return StageResult::fail("tls", e.to_string()),
    };
    let client = imap::Client::new(tls);
    imap_blocking_finish(client, username, password)
}

fn imap_blocking_starttls(
    _host: &str,
    _tcp: TcpStream,
    _username: &str,
    _password: &str,
) -> StageResult {
    // The `imap` crate's STARTTLS upgrade (`Client::secure`) is gated on
    // its `tls` feature, which we explicitly disable in the workspace
    // (`imap = { workspace = true, default-features = false }`) so we
    // can drive TLS through `rustls-connector` directly. None of the 11
    // providers in the hardcoded table use STARTTLS on IMAP (they all
    // use implicit SSL on port 993), so this branch is reachable only
    // for self-hosted servers configured manually through the Advanced
    // disclosure. Surface a clean error pointing the user at SSL/993 —
    // the error translator (`email_errors.rs:imap_starttls_unsupported`)
    // turns this into actionable copy.
    StageResult::fail(
        "tls",
        "IMAP STARTTLS is not supported in this build — please configure the server with implicit SSL/TLS on port 993",
    )
}

fn imap_blocking_plain(tcp: TcpStream, username: &str, password: &str) -> StageResult {
    let client = imap::Client::new(tcp);
    imap_blocking_finish(client, username, password)
}

fn imap_blocking_finish<S>(client: imap::Client<S>, username: &str, password: &str) -> StageResult
where
    S: std::io::Read + std::io::Write,
{
    // Stage 2: LOGIN — the ack is itself proof the credentials passed.
    // We used to follow up with `LIST "" ""` for paranoia, but that's
    // RFC 3501's special "return the hierarchy delimiter" form and some
    // servers (Dovecot configurations in the wild) reject it with
    // "Invalid pattern" even after a successful LOGIN, producing a false
    // negative. The legacy `test_email_connection` command never ran
    // LIST either, so dropping it restores parity.
    let mut session = match client.login(username, password) {
        Ok(s) => s,
        Err((e, _)) => return StageResult::fail("login", e.to_string()),
    };

    // Stage 3: LOGOUT. Failure here is cosmetic — the credentials worked.
    if let Err(e) = session.logout() {
        return StageResult::fail("logout", e.to_string());
    }

    StageResult::ok()
}

async fn test_smtp(config: &EmailTestConfig, password: &str) -> StageResult {
    // The FE derives `smtp_security` from a checkbox that can disagree
    // with the port (e.g. SSL + 587). Rather than fail the user with a
    // cryptic rustls "received corrupt message of type InvalidContentType"
    // — which is what happens when we TLS-handshake against a plaintext
    // SMTP banner — try the user's choice first, then fall back to the
    // port-implied choice if the failure smells like a TLS-record
    // mismatch. Auth / DNS / refused-connection errors are NOT retried.
    // Share the 465-implicit / 587-STARTTLS mapping with the real send path
    // (`athen-sentidos::email_send` → `Security::for_smtp_port`) so the test
    // wizard and the live sender can never disagree about which TLS mode a
    // port wants. For ports we don't have an opinion on, keep the user's
    // checkbox choice rather than forcing STARTTLS.
    let port_implied_security = match config.smtp_port {
        465 | 587 | 25 => Security::for_smtp_port(config.smtp_port, true),
        _ => config.smtp_security,
    };

    let primary = run_smtp_attempt(config, password, config.smtp_security).await;
    if primary.ok {
        return primary;
    }

    let looks_like_tls_mismatch = primary
        .error
        .as_deref()
        .map(|e| {
            let e = e.to_ascii_lowercase();
            e.contains("invalidcontenttype")
                || e.contains("corrupt message")
                || e.contains("alert received")
                || e.contains("unexpected eof")
        })
        .unwrap_or(false);

    if looks_like_tls_mismatch && port_implied_security != config.smtp_security {
        let retry = run_smtp_attempt(config, password, port_implied_security).await;
        if retry.ok {
            // Surface a synthetic stage so the FE can flip the checkbox
            // before persisting — see frontend/app.js's test-and-save
            // handler.
            return StageResult {
                ok: true,
                error: None,
                stage: Some("auto_corrected_security".to_string()),
            };
        }
        return retry;
    }

    primary
}

/// One end-to-end SMTP attempt with a specific `Security`. Same port,
/// creds, timeout — only the relay/starttls/dangerous choice differs.
/// Factored out so `test_smtp` can retry with a different Security
/// without duplicating the builder plumbing.
async fn run_smtp_attempt(
    config: &EmailTestConfig,
    password: &str,
    security: Security,
) -> StageResult {
    let creds = Credentials::new(config.smtp_username.clone(), password.to_string());

    let builder = match security {
        Security::Ssl => AsyncSmtpTransport::<Tokio1Executor>::relay(&config.smtp_host),
        Security::StartTls => {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.smtp_host)
        }
        Security::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(
            &config.smtp_host,
        )),
    };

    let transport: AsyncSmtpTransport<Tokio1Executor> = match builder {
        Ok(b) => b
            .port(config.smtp_port)
            .credentials(creds)
            .timeout(Some(Duration::from_secs(5)))
            .build(),
        Err(e) => return StageResult::fail("tls", e.to_string()),
    };

    // lettre's `test_connection` runs EHLO + AUTH LOGIN + RSET + QUIT
    // without sending mail. Any failure (refused TCP, TLS, bad creds,
    // server-side policy) surfaces here as a single error. We tag the
    // stage `auth` because that's overwhelmingly what fails in practice
    // — TCP/TLS reachability has already been proven in the IMAP half
    // for most providers (same host family).
    match transport.test_connection().await {
        Ok(true) => StageResult::ok(),
        Ok(false) => StageResult::fail("auth", "SMTP server refused the test connection"),
        Err(e) => {
            let stage = classify_smtp_error(&e);
            StageResult::fail(stage, e.to_string())
        }
    }
}

/// Bucket a lettre SMTP error into one of our stage labels. Best-effort —
/// lettre's error type doesn't expose enough structure to be perfect, so
/// we string-match the rendered display form against well-known prefixes
/// before falling back to `"auth"`.
fn classify_smtp_error(e: &lettre::transport::smtp::Error) -> &'static str {
    let msg = e.to_string().to_ascii_lowercase();
    if msg.contains("connect") || msg.contains("io error") || msg.contains("dns") {
        "tcp"
    } else if msg.contains("tls") || msg.contains("handshake") || msg.contains("certificate") {
        "tls"
    } else if msg.contains("ehlo") {
        "ehlo"
    } else {
        "auth"
    }
}
