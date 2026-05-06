//! SMTP outbound adapter implementing [`EmailSender`] via lettre.

use async_trait::async_trait;
use lettre::message::header::{ContentType, InReplyTo, References};
use lettre::message::{Mailbox, Mailboxes, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::PoolConfig;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use athen_core::config::EmailConfig;
use athen_core::error::{AthenError, Result};
use athen_core::traits::email_sender::{EmailSender, OutboundEmail, SentEmail};

/// SMTP-only slice of [`EmailConfig`]. Lets the adapter be constructed from
/// any source (UI, tests, fixtures) without dragging in IMAP fields.
#[derive(Debug, Clone)]
pub struct SmtpSettings {
    pub server: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    /// `true` = implicit TLS (port 465); `false` = STARTTLS (587).
    pub use_implicit_tls: bool,
    pub from_address: String,
}

impl SmtpSettings {
    pub fn from_email_config(cfg: &EmailConfig) -> Self {
        Self {
            server: cfg.smtp_server.clone(),
            port: cfg.smtp_port,
            username: cfg.smtp_username.clone(),
            password: cfg.smtp_password.clone(),
            use_implicit_tls: cfg.smtp_use_tls,
            from_address: cfg.from_address.clone(),
        }
    }
}

pub struct LettreSmtpSender {
    settings: SmtpSettings,
    transport: AsyncSmtpTransport<Tokio1Executor>,
}

impl LettreSmtpSender {
    pub fn new(settings: SmtpSettings) -> Result<Self> {
        if settings.server.is_empty() {
            return Err(AthenError::Config(
                "SMTP server address is empty".to_string(),
            ));
        }
        if settings.from_address.is_empty() {
            return Err(AthenError::Config("SMTP from_address is empty".to_string()));
        }

        let creds = Credentials::new(settings.username.clone(), settings.password.clone());
        let pool = PoolConfig::new();

        let builder = if settings.use_implicit_tls {
            AsyncSmtpTransport::<Tokio1Executor>::relay(&settings.server)
                .map_err(|e| AthenError::Other(format!("SMTP relay setup: {e}")))?
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&settings.server)
                .map_err(|e| AthenError::Other(format!("SMTP STARTTLS relay setup: {e}")))?
        };

        let transport = builder
            .port(settings.port)
            .credentials(creds)
            .pool_config(pool)
            .build();

        Ok(Self {
            settings,
            transport,
        })
    }
}

#[async_trait]
impl EmailSender for LettreSmtpSender {
    async fn send(&self, email: &OutboundEmail) -> Result<SentEmail> {
        let message = build_message(&self.settings, email)?;

        // Capture the auto-generated Message-ID before consuming the message
        // — lettre stamps it during builder finalization.
        let message_id = message
            .headers()
            .get_raw("Message-ID")
            .map(|s| s.trim().trim_matches(|c| c == '<' || c == '>').to_string())
            .unwrap_or_default();

        let envelope_to: Vec<String> = message
            .envelope()
            .to()
            .iter()
            .map(|addr| addr.to_string())
            .collect();

        self.transport
            .send(message)
            .await
            .map_err(|e| AthenError::Other(format!("SMTP send: {e}")))?;

        Ok(SentEmail {
            message_id,
            accepted_recipients: envelope_to,
        })
    }

    async fn test_connection(&self) -> Result<()> {
        let ok = self
            .transport
            .test_connection()
            .await
            .map_err(|e| AthenError::Other(format!("SMTP test_connection: {e}")))?;
        if !ok {
            return Err(AthenError::Other(
                "SMTP server did not accept the test connection".to_string(),
            ));
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "lettre-smtp"
    }
}

/// Build a [`lettre::Message`] from settings + an [`OutboundEmail`].
/// Extracted from `send()` so unit tests can inspect the constructed
/// message without a live transport.
pub(crate) fn build_message(settings: &SmtpSettings, email: &OutboundEmail) -> Result<Message> {
    if email.to.is_empty() {
        return Err(AthenError::Config(
            "OutboundEmail.to must contain at least one recipient".to_string(),
        ));
    }

    let from: Mailbox = settings
        .from_address
        .parse()
        .map_err(|e| AthenError::Config(format!("Invalid from_address: {e}")))?;

    let mut builder = Message::builder().from(from).subject(&email.subject);

    builder = parse_into(builder, &email.to, BuilderField::To)?;
    if !email.cc.is_empty() {
        builder = parse_into(builder, &email.cc, BuilderField::Cc)?;
    }
    if !email.bcc.is_empty() {
        builder = parse_into(builder, &email.bcc, BuilderField::Bcc)?;
    }

    if let Some(parent) = &email.in_reply_to {
        // RFC 5322: References should chain prior message-ids; for a fresh reply
        // we mirror In-Reply-To so the thread is identifiable even without the
        // upstream chain.
        let bare = parent
            .trim()
            .trim_matches(|c| c == '<' || c == '>')
            .to_string();
        builder = builder
            .header(InReplyTo::from(bare.clone()))
            .header(References::from(bare));
    }

    let message = if let Some(html) = &email.body_html {
        builder
            .multipart(MultiPart::alternative_plain_html(
                email.body_text.clone(),
                html.clone(),
            ))
            .map_err(|e| AthenError::Other(format!("Build multipart message: {e}")))?
    } else {
        builder
            .singlepart(
                SinglePart::builder()
                    .header(ContentType::TEXT_PLAIN)
                    .body(email.body_text.clone()),
            )
            .map_err(|e| AthenError::Other(format!("Build text message: {e}")))?
    };

    Ok(message)
}

enum BuilderField {
    To,
    Cc,
    Bcc,
}

fn parse_into(
    builder: lettre::message::MessageBuilder,
    addrs: &[String],
    field: BuilderField,
) -> Result<lettre::message::MessageBuilder> {
    let mut mailboxes = Mailboxes::new();
    for raw in addrs {
        let mb: Mailbox = raw
            .parse()
            .map_err(|e| AthenError::Config(format!("Invalid address '{raw}': {e}")))?;
        mailboxes.push(mb);
    }
    Ok(match field {
        BuilderField::To => builder.mailbox(lettre::message::header::To::from(mailboxes)),
        BuilderField::Cc => builder.mailbox(lettre::message::header::Cc::from(mailboxes)),
        BuilderField::Bcc => builder.mailbox(lettre::message::header::Bcc::from(mailboxes)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> SmtpSettings {
        SmtpSettings {
            server: "smtp.example.com".to_string(),
            port: 587,
            username: "user".to_string(),
            password: "pass".to_string(),
            use_implicit_tls: false,
            from_address: "Athen <athen@example.com>".to_string(),
        }
    }

    fn outbound() -> OutboundEmail {
        OutboundEmail {
            to: vec!["alice@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "Hello".to_string(),
            body_text: "Plain body".to_string(),
            body_html: None,
            in_reply_to: None,
        }
    }

    fn formatted(msg: &Message) -> String {
        String::from_utf8(msg.formatted()).expect("message is not valid UTF-8")
    }

    #[test]
    fn builds_basic_text_message() {
        let msg = build_message(&settings(), &outbound()).unwrap();
        let raw = formatted(&msg);
        assert!(raw.contains("From:"));
        assert!(raw.contains("Athen"));
        assert!(raw.contains("athen@example.com"));
        assert!(raw.contains("To: alice@example.com"));
        assert!(raw.contains("Subject: Hello"));
        assert!(raw.contains("Plain body"));
        assert!(raw.contains("text/plain"));
    }

    #[test]
    fn html_body_emits_multipart_alternative() {
        let mut email = outbound();
        email.body_html = Some("<p>Hi</p>".to_string());
        let msg = build_message(&settings(), &email).unwrap();
        let raw = formatted(&msg);
        assert!(raw.contains("multipart/alternative"));
        assert!(raw.contains("Plain body"));
        assert!(raw.contains("<p>Hi</p>"));
    }

    #[test]
    fn in_reply_to_emits_threading_headers() {
        let mut email = outbound();
        email.in_reply_to = Some("<abc123@mail.example.com>".to_string());
        let msg = build_message(&settings(), &email).unwrap();
        let raw = formatted(&msg);
        assert!(raw.contains("In-Reply-To:"));
        assert!(raw.contains("References:"));
        assert!(raw.contains("abc123@mail.example.com"));
    }

    #[test]
    fn supports_multiple_to_recipients() {
        let mut email = outbound();
        email.to = vec![
            "alice@example.com".to_string(),
            "bob@example.com".to_string(),
        ];
        let msg = build_message(&settings(), &email).unwrap();
        let envelope_to: Vec<String> = msg.envelope().to().iter().map(|a| a.to_string()).collect();
        assert_eq!(envelope_to.len(), 2);
        assert!(envelope_to.iter().any(|a| a == "alice@example.com"));
        assert!(envelope_to.iter().any(|a| a == "bob@example.com"));
    }

    #[test]
    fn empty_to_is_rejected() {
        let mut email = outbound();
        email.to.clear();
        let err = build_message(&settings(), &email).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("at least one recipient"),
            "unexpected error: {msg}"
        );
    }
}
