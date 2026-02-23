use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use indexmap::IndexMap;
use tokio::sync::Mutex;

use melib::backends::{BackendEventConsumer, IsSubscribedFn, MailBackend};
use melib::conf::AccountSettings;
use melib::email::attachment_types::{ContentType, Text};
use melib::imap::ImapType;
use melib::{AccountHash, EnvelopeHash, Mail, MailboxHash};

use crate::config::Config;
use crate::core::models::{Folder, MessageSummary};

/// A live IMAP session backed by melib.
pub struct ImapSession {
    backend: Arc<Mutex<Box<ImapType>>>,
    /// Map from mailbox hash to folder path (for lookups).
    mailbox_paths: Mutex<HashMap<MailboxHash, String>>,
}

impl std::fmt::Debug for ImapSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapSession").finish_non_exhaustive()
    }
}

impl ImapSession {
    /// Connect to the IMAP server using the given config.
    pub async fn connect(config: Config) -> Result<Arc<Self>, String> {
        let mut extra = IndexMap::new();
        extra.insert("server_hostname".into(), config.imap_server.clone());
        extra.insert("server_username".into(), config.username.clone());
        extra.insert("server_password".into(), config.password.clone());
        extra.insert("server_port".into(), config.imap_port.to_string());
        extra.insert("use_tls".into(), "true".into());
        extra.insert(
            "use_starttls".into(),
            if config.use_starttls {
                "true"
            } else {
                "false"
            }
            .into(),
        );
        extra.insert("danger_accept_invalid_certs".into(), "false".into());

        let account_settings = AccountSettings {
            name: config.username.clone(),
            root_mailbox: "INBOX".into(),
            format: "imap".into(),
            identity: config.username.clone(),
            extra,
            ..Default::default()
        };

        let is_subscribed: IsSubscribedFn =
            (Arc::new(|_: &str| true) as Arc<dyn Fn(&str) -> bool + Send + Sync>).into();

        let event_consumer = BackendEventConsumer::new(Arc::new(
            |_account_hash: AccountHash, event: melib::backends::BackendEvent| {
                log::debug!("IMAP backend event: {:?}", event);
            },
        ));

        let backend = ImapType::new(&account_settings, is_subscribed, event_consumer)
            .map_err(|e| format!("Failed to create IMAP backend: {}", e))?;

        let session = ImapSession {
            backend: Arc::new(Mutex::new(backend)),
            mailbox_paths: Mutex::new(HashMap::new()),
        };

        // Verify we can connect
        {
            let backend = session.backend.lock().await;
            let online_future = backend
                .is_online()
                .map_err(|e| format!("IMAP is_online failed: {}", e))?;
            online_future
                .await
                .map_err(|e| format!("IMAP connection failed: {}", e))?;
        }

        Ok(Arc::new(session))
    }

    /// Fetch the list of folders (mailboxes) from the server.
    pub async fn fetch_folders(self: &Arc<Self>) -> Result<Vec<Folder>, String> {
        let future = {
            let backend = self.backend.lock().await;
            backend
                .mailboxes()
                .map_err(|e| format!("Failed to request mailboxes: {}", e))?
        };

        let mailboxes = future
            .await
            .map_err(|e| format!("Failed to fetch mailboxes: {}", e))?;

        let mut folders: Vec<Folder> = Vec::with_capacity(mailboxes.len());
        let mut path_map = HashMap::new();

        for (hash, mailbox) in &mailboxes {
            let (total, unseen) = mailbox
                .count()
                .map_err(|e| format!("Failed to get mailbox count: {}", e))?;

            path_map.insert(*hash, mailbox.path().to_string());

            folders.push(Folder {
                name: mailbox.name().to_string(),
                path: mailbox.path().to_string(),
                unread_count: unseen as u32,
                total_count: total as u32,
                mailbox_hash: hash.0,
            });
        }

        // Sort: INBOX first, then alphabetical
        folders.sort_by(|a, b| {
            if a.path == "INBOX" {
                std::cmp::Ordering::Less
            } else if b.path == "INBOX" {
                std::cmp::Ordering::Greater
            } else {
                a.path.cmp(&b.path)
            }
        });

        *self.mailbox_paths.lock().await = path_map;

        Ok(folders)
    }

    /// Fetch message summaries (envelopes) for a mailbox.
    pub async fn fetch_messages(
        self: &Arc<Self>,
        mailbox_hash: MailboxHash,
    ) -> Result<Vec<MessageSummary>, String> {
        let stream = {
            let mut backend = self.backend.lock().await;
            backend
                .fetch(mailbox_hash)
                .map_err(|e| format!("Failed to start fetch: {}", e))?
        };

        let mut stream = std::pin::pin!(stream);
        let mut messages = Vec::new();

        while let Some(batch_result) = stream.next().await {
            let envelopes = batch_result
                .map_err(|e| format!("Error fetching envelopes: {}", e))?;

            for envelope in envelopes {
                let from_str = envelope
                    .from()
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");

                messages.push(MessageSummary {
                    uid: envelope.hash().0,
                    subject: envelope.subject().to_string(),
                    from: from_str,
                    date: envelope.date_as_str().to_string(),
                    is_read: envelope.is_seen(),
                    is_starred: envelope.flags().is_flagged(),
                    has_attachments: envelope.has_attachments,
                    thread_id: None,
                    envelope_hash: envelope.hash().0,
                    timestamp: envelope.timestamp as i64,
                });
            }
        }

        // Most recent first
        messages.reverse();

        Ok(messages)
    }

    /// Fetch and render the body of a single message.
    pub async fn fetch_body(
        self: &Arc<Self>,
        envelope_hash: EnvelopeHash,
    ) -> Result<String, String> {
        let future = {
            let backend = self.backend.lock().await;
            backend
                .envelope_bytes_by_hash(envelope_hash)
                .map_err(|e| format!("Failed to request message bytes: {}", e))?
        };

        let bytes = future
            .await
            .map_err(|e| format!("Failed to fetch message bytes: {}", e))?;

        let mail =
            Mail::new(bytes, None).map_err(|e| format!("Failed to parse message: {}", e))?;

        let body_attachment = mail.body();
        let (text_plain, text_html) = extract_body_text(&body_attachment);

        Ok(crate::core::mime::render_body(
            text_plain.as_deref(),
            text_html.as_deref(),
        ))
    }
}

/// Walk the MIME tree and extract text/plain and text/html parts.
fn extract_body_text(
    att: &melib::email::attachments::Attachment,
) -> (Option<String>, Option<String>) {
    let mut text_plain = None;
    let mut text_html = None;
    extract_parts(att, &mut text_plain, &mut text_html);
    (text_plain, text_html)
}

fn extract_parts(
    att: &melib::email::attachments::Attachment,
    plain: &mut Option<String>,
    html: &mut Option<String>,
) {
    match &att.content_type {
        ContentType::Text {
            kind: Text::Plain, ..
        } => {
            let bytes = att.decode(Default::default());
            *plain = Some(String::from_utf8_lossy(&bytes).to_string());
        }
        ContentType::Text {
            kind: Text::Html, ..
        } => {
            let bytes = att.decode(Default::default());
            *html = Some(String::from_utf8_lossy(&bytes).to_string());
        }
        ContentType::Multipart { parts, .. } => {
            for part in parts {
                extract_parts(part, plain, html);
            }
        }
        _ => {}
    }
}
