use std::path::PathBuf;

use cosmic::app::Task;
use cosmic::widget::{image, markdown};
use futures::future::{AbortHandle, Abortable};

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
    current_selected_email_id: Option<&str>,
    incoming_email_id: &str,
) -> bool {
    current_epoch == incoming_epoch
        && current_selected_email_id == Some(incoming_email_id)
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
                    let email_id = self.messages[index].email_id.clone();
                    cosmic::task::future(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        Message::AutoMarkRead(email_id)
                    })
                } else {
                    Task::none()
                };

                let body_task = if let Some(msg) = self.messages.get(index) {
                    let email_id = msg.email_id.clone();
                    let account_id = msg.account_id.clone();

                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        let client = self.client_for_account(&account_id);
                        let email_id_for_fetch = email_id.clone();
                        self.status_message = "Loading message...".into();
                        cosmic::task::future(async move {
                            let load = async move {
                                // Cache-first: try cache (includes attachments)
                                if let Ok(Some((md_body, plain_body, attachments))) =
                                    cache
                                        .load_body(account_id.clone(), email_id.clone())
                                        .await
                                {
                                    return Message::BodyLoaded {
                                        email_id,
                                        epoch: body_epoch,
                                        result: Ok((md_body, plain_body, attachments)),
                                    };
                                }

                                // Cache miss: fetch from JMAP, save to cache
                                if let Some(client) = client {
                                    let result = neverlight_mail_core::email::get_body(
                                        &client,
                                        &email_id_for_fetch,
                                    )
                                    .await
                                    .map_err(|e| e.to_string());
                                    match result {
                                        Ok((ref md_body, ref plain_body, ref attachments)) => {
                                            if let Err(e) = cache
                                                .save_body(
                                                    account_id.clone(),
                                                    email_id_for_fetch.clone(),
                                                    md_body.clone(),
                                                    plain_body.clone(),
                                                    attachments.clone(),
                                                )
                                                .await
                                            {
                                                log::warn!("Failed to cache body: {}", e);
                                            }
                                            Message::BodyLoaded {
                                                email_id: email_id_for_fetch,
                                                epoch: body_epoch,
                                                result,
                                            }
                                        }
                                        Err(_) => Message::BodyLoaded {
                                            email_id: email_id_for_fetch,
                                            epoch: body_epoch,
                                            result,
                                        },
                                    }
                                } else {
                                    // Client not ready yet — signal deferral
                                    Message::BodyDeferred {
                                        email_id: email_id_for_fetch,
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
                        // No-cache fallback: direct JMAP fetch
                        let client = self.client_for_account(&account_id);
                        if let Some(client) = client {
                            self.status_message = "Loading message...".into();
                            cosmic::task::future(async move {
                                let load = async move {
                                    Message::BodyLoaded {
                                        email_id: email_id.clone(),
                                        epoch: body_epoch,
                                        result: neverlight_mail_core::email::get_body(
                                            &client, &email_id,
                                        )
                                        .await
                                        .map_err(|e| e.to_string()),
                                    }
                                };
                                match Abortable::new(load, abort_reg).await {
                                    Ok(message) => message,
                                    Err(_) => Message::Noop,
                                }
                            })
                        } else {
                            // No cache, no client — defer until connected
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

            Message::BodyDeferred { email_id, epoch } => {
                let current_id = self
                    .selected_message
                    .and_then(|i| self.messages.get(i))
                    .map(|m| m.email_id.as_str());
                if !should_apply_body_result(self.body_epoch, epoch, current_id, &email_id) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.body_abort = None;
                if let Some(index) = self.selected_message {
                    const MAX_DEFER_RETRIES: u8 = 6;

                    if self.body_defer_retries < MAX_DEFER_RETRIES
                        && !self.is_busy()
                        && self.active_client().is_some()
                    {
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
                email_id,
                epoch,
                result: Ok((markdown_body, plain_body, attachments)),
            } => {
                let current_id = self
                    .selected_message
                    .and_then(|i| self.messages.get(i))
                    .map(|m| m.email_id.as_str());
                if !should_apply_body_result(self.body_epoch, epoch, current_id, &email_id) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.body_abort = None;
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
                email_id,
                epoch,
                result: Err(e),
            } => {
                let current_id = self
                    .selected_message
                    .and_then(|i| self.messages.get(i))
                    .map(|m| m.email_id.as_str());
                if !should_apply_body_result(self.body_epoch, epoch, current_id, &email_id) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.body_abort = None;
                if self.is_busy() {
                    if let Some(index) = self.selected_message {
                        log::debug!("Body fetch deferred (still syncing): {}", e);
                        self.pending_body = Some(index);
                        self.status_message = "Syncing...".into();
                        return Task::none();
                    }
                }

                // Stale message: cached TOC has it but server doesn't.
                if body_error_indicates_stale_message(&e) {
                    log::warn!(
                        "Evicting stale message {} (body error: {})",
                        email_id,
                        e
                    );
                    if let Some(pos) = self
                        .messages
                        .iter()
                        .position(|m| m.email_id == email_id)
                    {
                        let account_id = self.messages[pos].account_id.clone();
                        self.remove_message_optimistic(pos);
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let eid = email_id.clone();
                            if !account_id.is_empty() {
                                let evict_task = cosmic::task::future(async move {
                                    if let Err(e) = cache
                                        .remove_message(account_id, eid)
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
        assert!(should_apply_body_result(7, 7, Some("M42"), "M42"));
    }

    #[test]
    fn body_result_drops_when_epoch_is_stale() {
        assert!(!should_apply_body_result(8, 7, Some("M42"), "M42"));
    }

    #[test]
    fn body_result_drops_when_selection_has_switched() {
        assert!(!should_apply_body_result(7, 7, Some("M99"), "M42"));
        assert!(!should_apply_body_result(7, 7, None, "M42"));
    }
}
