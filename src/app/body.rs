use std::path::PathBuf;

use cosmic::app::Task;
use cosmic::widget::{image, markdown};
use futures::future::{AbortHandle, Abortable};
use neverlight_mail_core::EnvelopeHash;

use super::{AppModel, Message};

fn body_error_indicates_stale_message(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("not found")
        || lower.contains("deleted before you requested")
        || lower.contains("local cache")
}

fn should_apply_body_result(
    current_epoch: u64,
    incoming_epoch: u64,
    current_selected_envelope_hash: Option<u64>,
    incoming_envelope_hash: u64,
) -> bool {
    current_epoch == incoming_epoch
        && current_selected_envelope_hash == Some(incoming_envelope_hash)
}

impl AppModel {
    pub(super) fn handle_body(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ViewBody(index) => {
                self.selected_message = Some(index);
                self.pending_body = None;
                self.body_defer_retries = 0;
                self.auto_read_suppressed = false;
                self.body_epoch = self.body_epoch.saturating_add(1);
                let body_epoch = self.body_epoch;
                if let Some(handle) = self.body_abort.take() {
                    handle.abort();
                }
                let (abort_handle, abort_reg) = AbortHandle::new_pair();
                self.body_abort = Some(abort_handle);

                // Schedule auto-mark-read after 5 seconds if unread
                let auto_read_task = if self
                    .messages
                    .get(index)
                    .is_some_and(|m| !m.is_read)
                {
                    let envelope_hash = self.messages[index].envelope_hash;
                    cosmic::task::future(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        Message::AutoMarkRead(envelope_hash)
                    })
                } else {
                    Task::none()
                };

                let body_task = if let Some(msg) = self.messages.get(index) {
                    let envelope_hash = msg.envelope_hash;
                    let account_id = self
                        .active_account
                        .and_then(|i| self.accounts.get(i))
                        .filter(|a| a.folders.iter().any(|f| f.mailbox_hash == msg.mailbox_hash))
                        .map(|a| a.config.id.clone())
                        .or_else(|| {
                            self.account_for_mailbox(msg.mailbox_hash)
                                .and_then(|i| self.accounts.get(i))
                                .map(|a| a.config.id.clone())
                        });

                    if let Some(cache) = &self.cache {
                        let Some(account_id_for_cache) = account_id.clone() else {
                            let err = format!(
                                "Cannot access body cache: no account for mailbox {}",
                                msg.mailbox_hash
                            );
                            log::error!("{}", err);
                            self.status_message = err;
                            return auto_read_task;
                        };
                        let cache = cache.clone();
                        let session = account_id
                            .as_deref()
                            .and_then(|aid| {
                                self.session_for_account_mailbox(aid, msg.mailbox_hash)
                            })
                            .or_else(|| self.active_session());
                        self.status_message = "Loading message...".into();
                        cosmic::task::future(async move {
                            let load = async move {
                                // Unified cache-first: try cache (includes attachments)
                                if let Ok(Some((md_body, plain_body, attachments))) =
                                    cache
                                        .load_body(account_id_for_cache.clone(), envelope_hash)
                                        .await
                                {
                                    return Message::BodyLoaded {
                                        envelope_hash,
                                        epoch: body_epoch,
                                        result: Ok((md_body, plain_body, attachments)),
                                    };
                                }

                                // Cache miss: fetch from IMAP, save to cache
                                if let Some(session) = session {
                                    let result = session
                                        .fetch_body(EnvelopeHash(envelope_hash))
                                        .await;
                                    match result {
                                        Ok((ref md_body, ref plain_body, ref attachments)) => {
                                            if let Err(e) = cache
                                                .save_body(
                                                    account_id_for_cache.clone(),
                                                    envelope_hash,
                                                    md_body.clone(),
                                                    plain_body.clone(),
                                                    attachments.clone(),
                                                )
                                                .await
                                            {
                                                log::warn!("Failed to cache body: {}", e);
                                            }
                                            Message::BodyLoaded {
                                                envelope_hash,
                                                epoch: body_epoch,
                                                result,
                                            }
                                        }
                                        Err(ref e) if e.contains("not found") => {
                                            // melib hasn't ingested this envelope yet
                                            // (still syncing) — defer instead of erroring
                                            log::debug!("Body fetch deferred (envelope not yet in melib): {}", e);
                                            Message::BodyDeferred {
                                                envelope_hash,
                                                epoch: body_epoch,
                                            }
                                        }
                                        Err(_) => Message::BodyLoaded {
                                            envelope_hash,
                                            epoch: body_epoch,
                                            result,
                                        },
                                    }
                                } else {
                                    // Session not ready yet — signal deferral
                                    Message::BodyDeferred {
                                        envelope_hash,
                                        epoch: body_epoch,
                                    }
                                }
                            };

                            match Abortable::new(load, abort_reg).await {
                                Ok(message) => message,
                                Err(_) => Message::Noop,
                            }
                        })
                    } else {
                        // No-cache fallback: direct IMAP fetch
                        let session = account_id
                            .as_deref()
                            .and_then(|aid| {
                                self.session_for_account_mailbox(aid, msg.mailbox_hash)
                            })
                            .or_else(|| self.active_session());
                        if let Some(session) = session {
                            self.status_message = "Loading message...".into();
                            cosmic::task::future(async move {
                                let load = async move {
                                    Message::BodyLoaded {
                                        envelope_hash,
                                        epoch: body_epoch,
                                        result: session.fetch_body(EnvelopeHash(envelope_hash)).await,
                                    }
                                };
                                match Abortable::new(load, abort_reg).await {
                                    Ok(message) => message,
                                    Err(_) => Message::Noop,
                                }
                            })
                        } else {
                            // No cache, no session — defer until connected
                            self.pending_body = Some(index);
                            self.status_message = "Connecting...".into();
                            Task::none()
                        }
                    }
                } else {
                    Task::none()
                };

                return cosmic::task::batch(vec![body_task, auto_read_task]);
            }

            Message::BodyDeferred { envelope_hash, epoch } => {
                let current_hash = self
                    .selected_message
                    .and_then(|i| self.messages.get(i))
                    .map(|m| m.envelope_hash);
                if !should_apply_body_result(self.body_epoch, epoch, current_hash, envelope_hash) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.body_abort = None;
                // Cache missed and melib hasn't ingested the envelope yet.
                // If sync already finished, retry after a short delay to give melib
                // time to process. Otherwise defer until SyncMessagesComplete flushes.
                if let Some(index) = self.selected_message {
                    const MAX_DEFER_RETRIES: u8 = 6;

                    if self.body_defer_retries < MAX_DEFER_RETRIES
                        && !self.is_busy()
                        && self.active_session().is_some()
                    {
                        // Sync already completed — retry after a brief delay
                        self.body_defer_retries += 1;
                        self.status_message = "Loading message...".into();
                        return cosmic::task::future(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            Message::ViewBody(index)
                        });
                    }

                    if self.body_defer_retries >= MAX_DEFER_RETRIES {
                        let msg = "Message body unavailable — try refreshing the folder";
                        self.preview_markdown =
                            cosmic::widget::markdown::parse(msg).collect();
                        self.preview_body = msg.into();
                        self.status_message = msg.into();
                    } else {
                        self.pending_body = Some(index);
                        self.status_message =
                            "Syncing — message will load when ready...".into();
                    }
                }
            }

            Message::BodyLoaded {
                envelope_hash,
                epoch,
                result: Ok((markdown_body, plain_body, attachments)),
            } => {
                let current_hash = self
                    .selected_message
                    .and_then(|i| self.messages.get(i))
                    .map(|m| m.envelope_hash);
                if !should_apply_body_result(self.body_epoch, epoch, current_hash, envelope_hash) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.body_abort = None;
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
            Message::BodyLoaded {
                envelope_hash,
                epoch,
                result: Err(e),
            } => {
                let current_hash = self
                    .selected_message
                    .and_then(|i| self.messages.get(i))
                    .map(|m| m.envelope_hash);
                if !should_apply_body_result(self.body_epoch, epoch, current_hash, envelope_hash) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.body_abort = None;
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

                // Stale message: cached TOC has it but server doesn't.
                // Evict from the list and trigger a refresh to reconcile.
                if body_error_indicates_stale_message(&e) {
                    log::warn!(
                        "Evicting stale message {} (body error: {})",
                        envelope_hash,
                        e
                    );
                    if let Some(pos) = self
                        .messages
                        .iter()
                        .position(|m| m.envelope_hash == envelope_hash)
                    {
                        self.remove_message_optimistic(pos);
                        // Evict from cache too
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            if let Some(account_id) = self
                                .account_for_mailbox(
                                    self.selected_mailbox_hash.unwrap_or(0),
                                )
                                .and_then(|i| self.accounts.get(i))
                                .map(|a| a.config.id.clone())
                            {
                                let evict_task = cosmic::task::future(async move {
                                    if let Err(e) = cache
                                        .remove_message(account_id, envelope_hash)
                                        .await
                                    {
                                        log::warn!(
                                            "Failed to evict stale message from cache: {}",
                                            e
                                        );
                                    }
                                    Message::Noop
                                });
                                let refresh_task = self.dispatch(Message::Refresh);
                                return cosmic::task::batch(vec![evict_task, refresh_task]);
                            }
                        }
                        return self.dispatch(Message::Refresh);
                    }
                    self.selected_message = None;
                    self.preview_body.clear();
                    self.preview_markdown.clear();
                    self.preview_attachments.clear();
                    self.preview_image_handles.clear();
                    self.status_message = "Message no longer exists on server".into();
                    return self.dispatch(Message::Refresh);
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

#[cfg(test)]
mod tests {
    use super::should_apply_body_result;

    #[test]
    fn body_result_applies_when_epoch_and_selected_message_match() {
        assert!(should_apply_body_result(7, 7, Some(42), 42));
    }

    #[test]
    fn body_result_drops_when_epoch_is_stale() {
        assert!(!should_apply_body_result(8, 7, Some(42), 42));
    }

    #[test]
    fn body_result_drops_when_selection_has_switched() {
        assert!(!should_apply_body_result(7, 7, Some(99), 42));
        assert!(!should_apply_body_result(7, 7, None, 42));
    }
}
