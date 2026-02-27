use std::sync::Arc;

use cosmic::app::Task;
use futures::{SinkExt, StreamExt};
use neverlight_mail_core::{BackendEvent, RefreshEventKind, Flag};
use neverlight_mail_core::imap::ImapSession;
use neverlight_mail_core::store;

use super::{AppModel, ConnectionState, ImapWatchEvent, Message};

pub(super) fn imap_watch_stream(
    session: Arc<ImapSession>,
) -> impl futures::Stream<Item = ImapWatchEvent> {
    cosmic::iced_futures::stream::channel(50, move |mut output| async move {
        match session.watch().await {
            Ok(stream) => {
                let mut stream = std::pin::pin!(stream);
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(BackendEvent::Refresh(rev)) => {
                            match rev.kind {
                                RefreshEventKind::Create(envelope) => {
                                    let from = envelope
                                        .from()
                                        .iter()
                                        .map(|a| a.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    let _ = output
                                        .send(ImapWatchEvent::NewMessage {
                                            mailbox_hash: rev.mailbox_hash.0,
                                            subject: envelope.subject().to_string(),
                                            from,
                                        })
                                        .await;
                                }
                                RefreshEventKind::Remove(envelope_hash) => {
                                    let _ = output
                                        .send(ImapWatchEvent::MessageRemoved {
                                            mailbox_hash: rev.mailbox_hash.0,
                                            envelope_hash: envelope_hash.0,
                                        })
                                        .await;
                                }
                                RefreshEventKind::NewFlags(envelope_hash, (flag, _tags)) => {
                                    let is_read = flag.contains(Flag::SEEN);
                                    let is_starred = flag.contains(Flag::FLAGGED);
                                    let flags = store::flags_to_u8(is_read, is_starred);
                                    let _ = output
                                        .send(ImapWatchEvent::FlagsChanged {
                                            mailbox_hash: rev.mailbox_hash.0,
                                            envelope_hash: envelope_hash.0,
                                            flags,
                                        })
                                        .await;
                                }
                                RefreshEventKind::Rescan => {
                                    let _ = output
                                        .send(ImapWatchEvent::Rescan)
                                        .await;
                                }
                                other => {
                                    log::debug!(
                                        "Unhandled IMAP watch event kind: {:?}",
                                        other
                                    );
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = output
                                .send(ImapWatchEvent::WatchError(e.to_string()))
                                .await;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = output.send(ImapWatchEvent::WatchError(e)).await;
            }
        }
        let _ = output.send(ImapWatchEvent::WatchEnded).await;
    })
}

impl AppModel {
    pub(super) fn handle_watch(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ImapEvent(ref _account_id, ImapWatchEvent::NewMessage {
                mailbox_hash,
                subject,
                from,
            }) => {
                let notif_task = cosmic::task::future(async move {
                    let subj = subject;
                    let f = from;
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = notify_rust::Notification::new()
                            .summary(&format!("From: {}", f))
                            .body(&subj)
                            .icon("mail-message-new")
                            .timeout(5000)
                            .show();
                    })
                    .await;
                    Message::Noop
                });

                // If viewing a folder from this account that matches the mailbox, refresh
                if let Some(acct_idx) = self.active_account {
                    if let Some(fi) = self.selected_folder {
                        if let Some(folder) = self.accounts.get(acct_idx).and_then(|a| a.folders.get(fi)) {
                            if folder.mailbox_hash == mailbox_hash {
                                let refresh_task = self.dispatch(Message::Refresh);
                                return cosmic::task::batch(vec![notif_task, refresh_task]);
                            }
                        }
                    }
                }
                return notif_task;
            }
            Message::ImapEvent(_, ImapWatchEvent::MessageRemoved {
                mailbox_hash,
                envelope_hash,
            }) => {
                // Only act if we're viewing the affected mailbox
                let viewing_mailbox = self.active_account
                    .and_then(|ai| {
                        self.selected_folder.and_then(|fi| {
                            self.accounts.get(ai).and_then(|a| a.folders.get(fi))
                        })
                    })
                    .is_some_and(|f| f.mailbox_hash == mailbox_hash);

                if viewing_mailbox {
                    // Find and remove from messages list
                    if let Some(pos) = self.messages.iter().position(|m| m.envelope_hash == envelope_hash) {
                        self.messages.remove(pos);

                        // Adjust selection
                        match self.selected_message {
                            Some(sel) if sel == pos => {
                                // Selected message was removed — clear preview
                                self.selected_message = if self.messages.is_empty() {
                                    None
                                } else {
                                    Some(sel.min(self.messages.len() - 1))
                                };
                                self.preview_body.clear();
                                self.preview_markdown.clear();
                                self.preview_attachments.clear();
                                self.preview_image_handles.clear();
                            }
                            Some(sel) if sel > pos => {
                                self.selected_message = Some(sel - 1);
                            }
                            _ => {}
                        }

                        self.recompute_visible();
                    }

                    // Fire-and-forget cache cleanup
                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        return cosmic::task::future(async move {
                            if let Err(e) = cache.remove_message(envelope_hash).await {
                                log::warn!("Failed to remove message from cache: {}", e);
                            }
                            Message::Noop
                        });
                    }
                }
            }

            Message::ImapEvent(_, ImapWatchEvent::FlagsChanged {
                mailbox_hash,
                envelope_hash,
                flags,
            }) => {
                let viewing_mailbox = self.active_account
                    .and_then(|ai| {
                        self.selected_folder.and_then(|fi| {
                            self.accounts.get(ai).and_then(|a| a.folders.get(fi))
                        })
                    })
                    .is_some_and(|f| f.mailbox_hash == mailbox_hash);

                if viewing_mailbox {
                    let (is_read, is_starred) = store::flags_from_u8(flags);
                    if let Some(msg) = self.messages.iter_mut()
                        .find(|m| m.envelope_hash == envelope_hash)
                    {
                        msg.is_read = is_read;
                        msg.is_starred = is_starred;
                    }

                    // Sync server flags and clear any pending op in cache
                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        return cosmic::task::future(async move {
                            if let Err(e) = cache.clear_pending_op(envelope_hash, flags).await {
                                log::warn!("Failed to sync flags in cache: {}", e);
                            }
                            Message::Noop
                        });
                    }
                }
            }

            Message::ImapEvent(_, ImapWatchEvent::Rescan) => {
                return self.dispatch(Message::Refresh);
            }

            Message::ImapEvent(ref account_id, ImapWatchEvent::WatchError(ref e)) => {
                log::warn!("IMAP watch error for account: {}", e);
                if let Some(idx) = self.account_index(account_id) {
                    self.accounts[idx].conn_state = ConnectionState::Error(e.clone());
                    self.accounts[idx].session = None;
                    let aid = account_id.clone();
                    return cosmic::task::future(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        Message::ForceReconnect(aid)
                    });
                }
            }
            Message::ImapEvent(ref account_id, ImapWatchEvent::WatchEnded) => {
                log::info!("IMAP watch stream ended for account");
                if let Some(idx) = self.account_index(account_id) {
                    self.accounts[idx].conn_state = ConnectionState::Error("Connection lost".into());
                    self.accounts[idx].session = None;
                    let aid = account_id.clone();
                    return cosmic::task::future(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        Message::ForceReconnect(aid)
                    });
                }
            }

            _ => {}
        }
        Task::none()
    }
}
