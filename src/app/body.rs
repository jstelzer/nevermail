use std::path::PathBuf;

use cosmic::app::Task;
use cosmic::widget::{image, markdown};
use neverlight_mail_core::EnvelopeHash;

use super::{AppModel, Message};

impl AppModel {
    pub(super) fn handle_body(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ViewBody(index) => {
                self.selected_message = Some(index);
                self.pending_body = None;

                if let Some(msg) = self.messages.get(index) {
                    let envelope_hash = msg.envelope_hash;
                    let account_id = self
                        .account_for_mailbox(msg.mailbox_hash)
                        .and_then(|i| self.accounts.get(i))
                        .map(|a| a.config.id.clone());

                    if let Some(cache) = &self.cache {
                        let Some(account_id) = account_id.clone() else {
                            let err = format!(
                                "Cannot access body cache: no account for mailbox {}",
                                msg.mailbox_hash
                            );
                            log::error!("{}", err);
                            self.status_message = err;
                            return Task::none();
                        };
                        let cache = cache.clone();
                        let session = self.session_for_mailbox(msg.mailbox_hash)
                            .or_else(|| self.active_session());
                        self.status_message = "Loading message...".into();
                        return cosmic::task::future(async move {
                            // Unified cache-first: try cache (includes attachments)
                            if let Ok(Some((md_body, plain_body, attachments))) =
                                cache.load_body(account_id.clone(), envelope_hash).await
                            {
                                return Message::BodyLoaded(Ok((md_body, plain_body, attachments)));
                            }

                            // Cache miss: fetch from IMAP, save to cache
                            if let Some(session) = session {
                                let result = session
                                    .fetch_body(EnvelopeHash(envelope_hash))
                                    .await;
                                if let Ok((ref md_body, ref plain_body, ref attachments)) = result {
                                    if let Err(e) = cache
                                        .save_body(
                                            account_id.clone(),
                                            envelope_hash,
                                            md_body.clone(),
                                            plain_body.clone(),
                                            attachments.clone(),
                                        )
                                        .await
                                    {
                                        log::warn!("Failed to cache body: {}", e);
                                    }
                                }
                                Message::BodyLoaded(result)
                            } else {
                                // Session not ready yet — signal deferral
                                Message::BodyDeferred
                            }
                        });
                    }

                    // No-cache fallback: direct IMAP fetch
                    let session = self.session_for_mailbox(msg.mailbox_hash)
                        .or_else(|| self.active_session());
                    if let Some(session) = session {
                        self.status_message = "Loading message...".into();
                        return cosmic::task::future(async move {
                            Message::BodyLoaded(
                                session.fetch_body(EnvelopeHash(envelope_hash)).await,
                            )
                        });
                    }

                    // No cache, no session — defer until connected
                    self.pending_body = Some(index);
                    self.status_message = "Connecting...".into();
                }
            }

            Message::BodyDeferred => {
                // Cache missed and session wasn't ready — defer until connected
                if let Some(index) = self.selected_message {
                    self.pending_body = Some(index);
                    self.status_message = "Connecting...".into();
                }
            }

            Message::BodyLoaded(Ok((markdown_body, plain_body, attachments))) => {
                // Safety net: if clean_email_html still produces too many items
                // (the markdown widget has no virtualization), fall back to plain text.
                const MAX_MD_ITEMS: usize = 200;

                let items: Vec<markdown::Item> = markdown::parse(&markdown_body).collect();
                log::debug!(
                    "Markdown: {} bytes input, {} items parsed",
                    markdown_body.len(),
                    items.len()
                );

                if items.len() <= MAX_MD_ITEMS {
                    self.preview_markdown = items;
                } else {
                    log::warn!(
                        "Markdown items ({}) exceed cap ({}), falling back to plain text",
                        items.len(),
                        MAX_MD_ITEMS
                    );
                    // Plain text through markdown::parse produces ~1 item per paragraph
                    self.preview_markdown = markdown::parse(&plain_body).collect();
                }
                self.preview_body = plain_body;
                self.preview_image_handles = attachments
                    .iter()
                    .map(|a| {
                        if a.is_image() {
                            Some(image::Handle::from_bytes(a.data.clone()))
                        } else {
                            None
                        }
                    })
                    .collect();
                self.preview_attachments = attachments;
                self.status_message = "Ready".into();
            }
            Message::BodyLoaded(Err(e)) => {
                // If still syncing, melib's store may not have the envelope yet.
                // Defer the fetch instead of showing an error.
                if self.is_busy() {
                    if let Some(index) = self.selected_message {
                        log::debug!("Body fetch deferred (still syncing): {}", e);
                        self.pending_body = Some(index);
                        self.status_message = "Syncing...".into();
                        return Task::none();
                    }
                }
                let msg = format!("Failed to load message body: {}", e);
                self.preview_markdown = markdown::parse(&msg).collect();
                self.preview_body = msg;
                self.status_message = "Error loading message".into();
                log::error!("Body fetch failed: {}", e);
            }

            Message::LinkClicked(url) => {
                neverlight_mail_core::mime::open_link(url.as_str());
            }

            Message::CopyBody => {
                if !self.preview_body.is_empty() {
                    return cosmic::iced::clipboard::write(self.preview_body.clone());
                }
            }

            Message::SaveAttachment(index) => {
                if let Some(att) = self.preview_attachments.get(index) {
                    let filename = att.filename.clone();
                    let data = att.data.clone();
                    return cosmic::task::future(async move {
                        let dir = dirs::download_dir()
                            .unwrap_or_else(|| PathBuf::from("."));
                        let path = dir.join(&filename);
                        match tokio::fs::write(&path, &data).await {
                            Ok(()) => Message::SaveAttachmentComplete(
                                Ok(path.display().to_string()),
                            ),
                            Err(e) => Message::SaveAttachmentComplete(
                                Err(format!("Save failed: {e}")),
                            ),
                        }
                    });
                }
            }

            Message::SaveAttachmentComplete(Ok(path)) => {
                self.status_message = format!("Saved to {path}");
            }
            Message::SaveAttachmentComplete(Err(e)) => {
                self.status_message = e;
                log::error!("Attachment save failed: {}", self.status_message);
            }

            _ => {}
        }
        Task::none()
    }
}
