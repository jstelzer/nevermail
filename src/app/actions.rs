use cosmic::app::Task;
use nevermail_core::{EnvelopeHash, FlagOp, Flag, MailboxHash};
use nevermail_core::store;

use super::{AppModel, Message};

impl AppModel {
    pub(super) fn handle_actions(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ToggleRead(index) => {
                if let Some(msg) = self.messages.get_mut(index) {
                    let new_read = !msg.is_read;
                    msg.is_read = new_read;
                    let envelope_hash = msg.envelope_hash;
                    let mailbox_hash = msg.mailbox_hash;
                    let new_flags = store::flags_to_u8(new_read, msg.is_starred);
                    let pending_op = if new_read { "set_seen" } else { "unset_seen" }.to_string();

                    let mut tasks: Vec<Task<Message>> = Vec::new();

                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        let op = pending_op.clone();
                        tasks.push(cosmic::task::future(async move {
                            if let Err(e) = cache.update_flags(envelope_hash, new_flags, op).await {
                                log::warn!("Failed to update cache flags: {}", e);
                            }
                            Message::Noop
                        }));
                    }

                    if let Some(session) = self.session_for_mailbox(mailbox_hash) {
                        let flag_op = if new_read {
                            FlagOp::Set(Flag::SEEN)
                        } else {
                            FlagOp::UnSet(Flag::SEEN)
                        };
                        tasks.push(cosmic::task::future(async move {
                            let result = session
                                .set_flags(
                                    EnvelopeHash(envelope_hash),
                                    MailboxHash(mailbox_hash),
                                    vec![flag_op],
                                )
                                .await;
                            Message::FlagOpComplete {
                                envelope_hash,
                                result: result.map(|_| new_flags),
                            }
                        }));
                    }

                    if !tasks.is_empty() {
                        return cosmic::task::batch(tasks);
                    }
                }
            }

            Message::ToggleStar(index) => {
                if let Some(msg) = self.messages.get_mut(index) {
                    let new_starred = !msg.is_starred;
                    msg.is_starred = new_starred;
                    let envelope_hash = msg.envelope_hash;
                    let mailbox_hash = msg.mailbox_hash;
                    let new_flags = store::flags_to_u8(msg.is_read, new_starred);
                    let pending_op = if new_starred { "set_flagged" } else { "unset_flagged" }.to_string();

                    let mut tasks: Vec<Task<Message>> = Vec::new();

                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        let op = pending_op.clone();
                        tasks.push(cosmic::task::future(async move {
                            if let Err(e) = cache.update_flags(envelope_hash, new_flags, op).await {
                                log::warn!("Failed to update cache flags: {}", e);
                            }
                            Message::Noop
                        }));
                    }

                    if let Some(session) = self.session_for_mailbox(mailbox_hash) {
                        let flag_op = if new_starred {
                            FlagOp::Set(Flag::FLAGGED)
                        } else {
                            FlagOp::UnSet(Flag::FLAGGED)
                        };
                        tasks.push(cosmic::task::future(async move {
                            let result = session
                                .set_flags(
                                    EnvelopeHash(envelope_hash),
                                    MailboxHash(mailbox_hash),
                                    vec![flag_op],
                                )
                                .await;
                            Message::FlagOpComplete {
                                envelope_hash,
                                result: result.map(|_| new_flags),
                            }
                        }));
                    }

                    if !tasks.is_empty() {
                        return cosmic::task::batch(tasks);
                    }
                }
            }

            Message::Trash(index) => {
                if let Some(msg) = self.messages.get(index) {
                    let mailbox_hash = msg.mailbox_hash;
                    if let Some(folder_map) = self.folder_map_for_mailbox(mailbox_hash) {
                        if let Some(trash_hash) = folder_map.get("Trash").or_else(|| folder_map.get("INBOX.Trash")).copied() {
                            let envelope_hash = msg.envelope_hash;
                            let source_mailbox = msg.mailbox_hash;
                            self.remove_message_optimistic(index);
                            return self.dispatch_move(envelope_hash, source_mailbox, trash_hash);
                        }
                    }
                    self.status_message = "Trash folder not found".into();
                }
            }

            Message::Archive(index) => {
                if let Some(msg) = self.messages.get(index) {
                    let mailbox_hash = msg.mailbox_hash;
                    if let Some(folder_map) = self.folder_map_for_mailbox(mailbox_hash) {
                        if let Some(archive_hash) = folder_map.get("Archive").or_else(|| folder_map.get("INBOX.Archive")).copied() {
                            let envelope_hash = msg.envelope_hash;
                            let source_mailbox = msg.mailbox_hash;
                            self.remove_message_optimistic(index);
                            return self.dispatch_move(envelope_hash, source_mailbox, archive_hash);
                        }
                    }
                    self.status_message = "Archive folder not found".into();
                }
            }

            Message::DragMessageToFolder {
                envelope_hash,
                source_mailbox,
                dest_mailbox,
            } => {
                self.folder_drag_target = None;

                // No-op if dragged onto the same folder
                if source_mailbox == dest_mailbox {
                    return Task::none();
                }

                // Prevent cross-account moves (IMAP MOVE is intra-server only)
                let src_acct = self.account_for_mailbox(source_mailbox);
                let dst_acct = self.account_for_mailbox(dest_mailbox);
                if src_acct != dst_acct {
                    self.status_message = "Cannot move messages between accounts".into();
                    return Task::none();
                }

                if let Some(index) = self.messages.iter().position(|m| m.envelope_hash == envelope_hash) {
                    self.remove_message_optimistic(index);
                    return self.dispatch_move(envelope_hash, source_mailbox, dest_mailbox);
                }
            }

            Message::FolderDragEnter(i) => {
                self.folder_drag_target = Some(i);
            }
            Message::FolderDragLeave => {
                self.folder_drag_target = None;
            }

            Message::FlagOpComplete {
                envelope_hash,
                result,
            } => {
                match result {
                    Ok(new_flags) => {
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            return cosmic::task::future(async move {
                                if let Err(e) = cache.clear_pending_op(envelope_hash, new_flags).await {
                                    log::warn!("Failed to clear pending op: {}", e);
                                }
                                Message::Noop
                            });
                        }
                    }
                    Err(e) => {
                        log::error!("Flag operation failed: {}", e);
                        self.status_message = format!("Flag update failed: {}", e);

                        // Revert optimistic UI
                        if let Some(msg) = self.messages.iter_mut().find(|m| m.envelope_hash == envelope_hash) {
                            msg.is_read = !msg.is_read; // toggle back
                        }

                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            return cosmic::task::future(async move {
                                if let Err(e) = cache.revert_pending_op(envelope_hash).await {
                                    log::warn!("Failed to revert pending op: {}", e);
                                }
                                Message::Noop
                            });
                        }
                    }
                }
            }

            Message::MoveOpComplete {
                envelope_hash,
                result,
            } => {
                match result {
                    Ok(()) => {
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
                    Err(e) => {
                        log::error!("Move operation failed: {}", e);
                        self.status_message = format!("Move failed: {}", e);
                    }
                }
            }

            _ => {}
        }
        Task::none()
    }

    /// Optimistically remove a message from the list and adjust selection.
    fn remove_message_optimistic(&mut self, index: usize) {
        self.messages.remove(index);
        if let Some(sel) = &mut self.selected_message {
            if *sel >= self.messages.len() && !self.messages.is_empty() {
                *sel = self.messages.len() - 1;
            } else if self.messages.is_empty() {
                self.selected_message = None;
                self.preview_body.clear();
                self.preview_markdown.clear();
                self.preview_attachments.clear();
                self.preview_image_handles.clear();
            }
        }
        self.recompute_visible();
    }

    /// Dispatch IMAP move + cache update tasks for a message move operation.
    fn dispatch_move(
        &self,
        envelope_hash: u64,
        source_mailbox: u64,
        dest_mailbox: u64,
    ) -> Task<Message> {
        let mut tasks: Vec<Task<Message>> = Vec::new();

        if let Some(cache) = &self.cache {
            let cache = cache.clone();
            let new_flags = store::flags_to_u8(true, false);
            tasks.push(cosmic::task::future(async move {
                if let Err(e) = cache
                    .update_flags(envelope_hash, new_flags, format!("move:{}", dest_mailbox))
                    .await
                {
                    log::warn!("Failed to update cache for move: {}", e);
                }
                Message::Noop
            }));
        }

        if let Some(session) = self.session_for_mailbox(source_mailbox) {
            tasks.push(cosmic::task::future(async move {
                let result = session
                    .move_messages(
                        EnvelopeHash(envelope_hash),
                        MailboxHash(source_mailbox),
                        MailboxHash(dest_mailbox),
                    )
                    .await;
                Message::MoveOpComplete {
                    envelope_hash,
                    result,
                }
            }));
        }

        if tasks.is_empty() {
            Task::none()
        } else {
            cosmic::task::batch(tasks)
        }
    }
}
