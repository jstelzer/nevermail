use std::path::PathBuf;

use cosmic::app::Task;
use cosmic::widget::{image, markdown};
use futures::future::{AbortHandle, Abortable};

use super::{AppModel, ConversationEntry, Message};
use neverlight_mail_core::models::AttachmentData;
use neverlight_mail_core::store::CacheHandle;

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

const MAX_MD_ITEMS: usize = 200;

fn parse_markdown_capped(markdown_body: &str, plain_body: &str) -> Vec<markdown::Item> {
    let items: Vec<markdown::Item> = markdown::parse(markdown_body).collect();
    if items.len() <= MAX_MD_ITEMS {
        items
    } else {
        log::warn!(
            "Markdown items ({}) exceed cap ({}), falling back to plain text",
            items.len(),
            MAX_MD_ITEMS
        );
        markdown::parse(plain_body).collect()
    }
}

fn build_image_handles(attachments: &[AttachmentData]) -> Vec<Option<image::Handle>> {
    attachments
        .iter()
        .map(|a| {
            if a.is_image() {
                Some(image::Handle::from_bytes(a.data.clone()))
            } else {
                None
            }
        })
        .collect()
}

/// Build a body fetch task for a single email, returning either BodyLoaded or
/// ConversationBodyLoaded depending on whether `thread_id` is Some.
fn body_fetch_task(
    cache: Option<CacheHandle>,
    client: Option<neverlight_mail_core::client::JmapClient>,
    account_id: String,
    email_id: String,
    epoch: u64,
    thread_id: Option<String>,
) -> Task<Message> {
    if let Some(cache) = cache {
        let email_id_for_fetch = email_id.clone();
        cosmic::task::future(async move {
            // Cache-first
            if let Ok(Some((md_body, plain_body, attachments))) =
                cache
                    .load_body(account_id.clone(), email_id.clone())
                    .await
            {
                let result = Ok((md_body, plain_body, attachments));
                return if let Some(tid) = thread_id {
                    Message::ConversationBodyLoaded {
                        thread_id: tid,
                        email_id,
                        epoch,
                        result,
                    }
                } else {
                    Message::BodyLoaded {
                        email_id,
                        epoch,
                        result,
                    }
                };
            }

            // Cache miss: fetch from JMAP
            let Some(client) = client else {
                return if thread_id.is_some() {
                    // For conversation bodies, just report error (no defer mechanism)
                    Message::ConversationBodyLoaded {
                        thread_id: thread_id.unwrap_or_default(),
                        email_id: email_id_for_fetch,
                        epoch,
                        result: Err("Not connected".into()),
                    }
                } else {
                    Message::BodyDeferred {
                        email_id: email_id_for_fetch,
                        epoch,
                    }
                };
            };
            let result = neverlight_mail_core::email::get_body(&client, &email_id_for_fetch)
                .await
                .map_err(|e| e.to_string());
            if let Ok((ref md_body, ref plain_body, ref attachments)) = result {
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
            }
            if let Some(tid) = thread_id {
                Message::ConversationBodyLoaded {
                    thread_id: tid,
                    email_id: email_id_for_fetch,
                    epoch,
                    result,
                }
            } else {
                Message::BodyLoaded {
                    email_id: email_id_for_fetch,
                    epoch,
                    result,
                }
            }
        })
    } else {
        // No-cache fallback
        cosmic::task::future(async move {
            let result = if let Some(client) = client {
                neverlight_mail_core::email::get_body(&client, &email_id)
                    .await
                    .map_err(|e| e.to_string())
            } else {
                Err("Not connected".into())
            };
            if let Some(tid) = thread_id {
                Message::ConversationBodyLoaded {
                    thread_id: tid,
                    email_id,
                    epoch,
                    result,
                }
            } else {
                Message::BodyLoaded {
                    email_id,
                    epoch,
                    result,
                }
            }
        })
    }
}

impl AppModel {
    /// Check if a from address matches any of the account's email addresses.
    fn is_sent_message(&self, from: &str, account_id: &str) -> bool {
        let from_lower = from.to_lowercase();
        self.account_index(account_id)
            .and_then(|idx| self.accounts.get(idx))
            .is_some_and(|acct| {
                acct.config
                    .email_addresses
                    .iter()
                    .any(|addr| from_lower.contains(&addr.to_lowercase()))
            })
    }

    fn clear_conversation(&mut self) {
        self.conversation.clear();
        self.active_conversation_id = None;
    }

    /// Get inbox + sent mailbox IDs for the given account (for cross-mailbox thread queries).
    fn inbox_and_sent_mailbox_ids(&self, account_id: &str) -> Vec<String> {
        let Some(idx) = self.account_index(account_id) else {
            return Vec::new();
        };
        let Some(acct) = self.accounts.get(idx) else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        if let Some(id) = neverlight_mail_core::mailbox::find_by_role(&acct.folders, "inbox") {
            ids.push(id);
        }
        if let Some(id) = neverlight_mail_core::mailbox::find_by_role(&acct.folders, "sent") {
            ids.push(id);
        }
        ids
    }

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

                let Some(msg) = self.messages.get(index) else {
                    self.clear_conversation();
                    return auto_read_task;
                };
                let email_id = msg.email_id.clone();
                let account_id = msg.account_id.clone();

                // If this message has a thread_id, query the cache for the
                // full thread across INBOX + Sent (cross-mailbox).
                if let (Some(thread_id), Some(cache)) =
                    (msg.thread_id.clone(), self.cache.clone())
                {
                    // Resolve inbox + sent mailbox IDs for cross-mailbox thread query
                    let mailbox_ids = self.inbox_and_sent_mailbox_ids(&account_id);

                    if !mailbox_ids.is_empty() {
                        let eid = email_id.clone();
                        let tid = thread_id.clone();
                        let aid = account_id.clone();
                        self.status_message = "Loading conversation...".into();

                        let thread_task = cosmic::task::future(async move {
                            let result = cache
                                .load_thread(aid, tid.clone(), mailbox_ids)
                                .await;
                            Message::ThreadLoaded {
                                thread_id: tid,
                                email_id: eid,
                                epoch: body_epoch,
                                result,
                            }
                        });
                        return cosmic::task::batch(vec![thread_task, auto_read_task]);
                    }
                }

                // --- Single-message path (no thread_id or no cache) ---
                self.clear_conversation();
                let (abort_handle, abort_reg) = AbortHandle::new_pair();
                self.body_abort = Some(abort_handle);

                let body_task = if let Some(cache) = &self.cache {
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
                            let Some(client) = client else {
                                return Message::BodyDeferred {
                                    email_id: email_id_for_fetch,
                                    epoch: body_epoch,
                                };
                            };
                            let result = neverlight_mail_core::email::get_body(
                                &client,
                                &email_id_for_fetch,
                            )
                            .await
                            .map_err(|e| e.to_string());
                            if let Ok((ref md_body, ref plain_body, ref attachments)) = result {
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
                            }
                            Message::BodyLoaded {
                                email_id: email_id_for_fetch,
                                epoch: body_epoch,
                                result,
                            }
                        };

                        match Abortable::new(load, abort_reg).await {
                            Ok(message) => message,
                            Err(_) => Message::Noop,
                        }
                    })
                } else {
                    // No-cache fallback: direct JMAP fetch
                    let Some(client) = self.client_for_account(&account_id) else {
                        self.pending_body = Some(index);
                        self.status_message = "Connecting...".into();
                        return auto_read_task;
                    };
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

                self.preview_markdown = parse_markdown_capped(&markdown_body, &plain_body);
                self.preview_body = plain_body;
                self.preview_image_handles = build_image_handles(&attachments);
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

            Message::ThreadLoaded {
                thread_id,
                email_id,
                epoch,
                result,
            } => {
                if self.body_epoch != epoch {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                let thread_msgs = match result {
                    Ok(msgs) => msgs,
                    Err(e) => {
                        log::warn!("Thread load failed: {}", e);
                        // Fall through to single-message view
                        self.clear_conversation();
                        return self.dispatch(Message::ViewBody(
                            self.selected_message.unwrap_or(0),
                        ));
                    }
                };

                if thread_msgs.len() <= 1 {
                    // Single message or empty — use normal single-message path
                    self.clear_conversation();

                    let account_id = thread_msgs
                        .first()
                        .map(|m| m.account_id.clone())
                        .or_else(|| {
                            self.selected_message
                                .and_then(|i| self.messages.get(i))
                                .map(|m| m.account_id.clone())
                        })
                        .unwrap_or_default();

                    self.status_message = "Loading message...".into();
                    return body_fetch_task(
                        self.cache.clone(),
                        self.client_for_account(&account_id),
                        account_id,
                        email_id,
                        epoch,
                        None,
                    );
                }

                // --- Multi-message conversation ---
                self.active_conversation_id = Some(email_id.clone());

                self.conversation = thread_msgs
                    .iter()
                    .map(|m| ConversationEntry {
                        email_id: m.email_id.clone(),
                        summary: m.clone(),
                        markdown_items: Vec::new(),
                        plain_body: String::new(),
                        attachments: Vec::new(),
                        image_handles: Vec::new(),
                        is_sent: self.is_sent_message(&m.from, &m.account_id),
                        loaded: false,
                    })
                    .collect();

                self.preview_markdown.clear();
                self.preview_body.clear();
                self.preview_attachments.clear();
                self.preview_image_handles.clear();

                let cache = self.cache.clone();
                let account_id = thread_msgs
                    .first()
                    .map(|m| m.account_id.clone())
                    .unwrap_or_default();
                let client = self.client_for_account(&account_id);
                let tasks: Vec<Task<Message>> = thread_msgs
                    .iter()
                    .map(|m| {
                        body_fetch_task(
                            cache.clone(),
                            client.clone(),
                            m.account_id.clone(),
                            m.email_id.clone(),
                            epoch,
                            Some(thread_id.clone()),
                        )
                    })
                    .collect();

                self.status_message = format!(
                    "Loading conversation ({} messages)...",
                    thread_msgs.len()
                );
                return cosmic::task::batch(tasks);
            }

            Message::ConversationBodyLoaded {
                thread_id,
                email_id,
                epoch,
                result,
            } => {
                // Stale check: epoch must match and current thread must match
                if self.body_epoch != epoch {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                let current_thread = self
                    .selected_message
                    .and_then(|i| self.messages.get(i))
                    .and_then(|m| m.thread_id.as_deref());
                if current_thread != Some(&thread_id) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }

                let Some(entry) = self
                    .conversation
                    .iter_mut()
                    .find(|e| e.email_id == email_id)
                else {
                    return Task::none();
                };

                match result {
                    Ok((markdown_body, plain_body, attachments)) => {
                        entry.markdown_items =
                            parse_markdown_capped(&markdown_body, &plain_body);
                        entry.image_handles = build_image_handles(&attachments);
                        entry.plain_body = plain_body;
                        entry.attachments = attachments;
                        entry.loaded = true;
                    }
                    Err(e) => {
                        let msg = format!("Failed to load: {e}");
                        entry.markdown_items = markdown::parse(&msg).collect();
                        entry.loaded = true;
                    }
                }

                // Update status when all entries are loaded
                let loaded_count = self.conversation.iter().filter(|e| e.loaded).count();
                let total = self.conversation.len();
                if loaded_count == total {
                    self.status_message = "Ready".into();
                } else {
                    self.status_message = format!(
                        "Loading conversation ({}/{})...",
                        loaded_count, total
                    );
                }
            }

            Message::SetActiveConversation(ref email_id) => {
                self.active_conversation_id = Some(email_id.clone());
                // Update selected_message to match so toolbar actions target the right message
                if let Some(idx) = self
                    .messages
                    .iter()
                    .position(|m| m.email_id == *email_id)
                {
                    self.selected_message = Some(idx);
                }
            }

            Message::SaveConversationAttachment {
                ref email_id,
                index,
            } => {
                let att = self
                    .conversation
                    .iter()
                    .find(|e| e.email_id == *email_id)
                    .and_then(|e| e.attachments.get(index));
                if let Some(att) = att {
                    let filename = att.filename.clone();
                    let data = att.data.clone();
                    return cosmic::task::future(async move {
                        let dir =
                            dirs::download_dir().unwrap_or_else(|| PathBuf::from("."));
                        let path = dir.join(&filename);
                        match tokio::fs::write(&path, &data).await {
                            Ok(()) => Message::SaveAttachmentComplete(Ok(
                                path.display().to_string(),
                            )),
                            Err(e) => Message::SaveAttachmentComplete(Err(format!(
                                "Save failed: {e}"
                            ))),
                        }
                    });
                }
            }

            Message::LinkClicked(url) => {
                neverlight_mail_core::mime::open_link(url.as_str());
            }

            Message::CopyBody => {
                // In conversation mode, copy the active entry's body
                if !self.conversation.is_empty() {
                    if let Some(ref active_id) = self.active_conversation_id {
                        if let Some(entry) = self
                            .conversation
                            .iter()
                            .find(|e| &e.email_id == active_id)
                        {
                            if !entry.plain_body.is_empty() {
                                return cosmic::iced::clipboard::write(
                                    entry.plain_body.clone(),
                                );
                            }
                        }
                    }
                    return Task::none();
                }
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
