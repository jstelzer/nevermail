use lettre::message::header;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

use crate::config::SmtpConfig;

pub struct OutgoingEmail {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub in_reply_to: Option<String>,
    pub references: Option<String>,
}

pub async fn send_email(config: &SmtpConfig, email: &OutgoingEmail) -> Result<(), String> {
    let from = email
        .from
        .parse()
        .map_err(|e| format!("Invalid From address: {e}"))?;

    let mut to_addrs = Vec::new();
    for addr in email.to.split(',') {
        let addr = addr.trim();
        if !addr.is_empty() {
            to_addrs.push(
                addr.parse()
                    .map_err(|e| format!("Invalid To address '{addr}': {e}"))?,
            );
        }
    }
    if to_addrs.is_empty() {
        return Err("No recipients specified".into());
    }

    let mut builder = Message::builder()
        .from(from)
        .subject(&email.subject);

    for addr in to_addrs {
        builder = builder.to(addr);
    }

    if let Some(ref irt) = email.in_reply_to {
        builder = builder.header(header::InReplyTo::from(irt.clone()));
    }
    if let Some(ref refs) = email.references {
        builder = builder.header(header::References::from(refs.clone()));
    }

    let message = builder
        .body(email.body.clone())
        .map_err(|e| format!("Failed to build message: {e}"))?;

    let creds = Credentials::new(config.username.clone(), config.password.clone());

    let transport = if config.use_starttls {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&config.server)
            .map_err(|e| format!("SMTP relay error: {e}"))?
            .port(config.port)
            .credentials(creds)
            .build()
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&config.server)
            .map_err(|e| format!("SMTP relay error: {e}"))?
            .port(config.port)
            .credentials(creds)
            .build()
    };

    transport
        .send(message)
        .await
        .map_err(|e| format!("SMTP send failed: {e}"))?;

    Ok(())
}
