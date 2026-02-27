use cosmic::app::Task;
use neverlight_mail_core::{EnvelopeHash, FlagOp, Flag, MailboxHash};
use neverlight_mail_core::store;

use super::{AppModel, Message};

impl AppModel {
    pub(super) fn handle_actions(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ToggleRead(index) => {
                if let Some(msg) = self.messages.get_mut(index) {
                    let prev_flags = store::flags_to_u8(msg.is_read, msg.is_starred);
                    let new_read = !msg.is_read;
                    msg.is_read = new_read;
                    let envelope_hash = msg.envelope_hash;
                    let mailbox_hash = msg.mailbox_hash;
                    let new_flags = store::flags_to_u8(new_read, msg.is_starred);
                    let pending_op = if new_read { "set_seen" } else { "unset_seen" }.to_string();

                    let mut tasks: Vec<Task<Message>> = Vec::new();

                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        let Some(account_id) = self
                            .account_for_mailbox(mailbox_hash)
                            .and_then(|i| self.accounts.get(i))
                            .map(|a| a.config.id.clone())
                        else {
                            let err = format!(
                                "Cannot update cache flags: no account for mailbox {}",
                                mailbox_hash
                            );
                            log::error!("{}", err);
                            self.status_message = err;
                            return Task::none();
                        };
                        let op = pending_op.clone();
                        tasks.push(cosmic::task::future(async move {
                            if let Err(e) = cache
                                .update_flags(account_id, envelope_hash, new_flags, op)
                                .await
                            {
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
                                prev_flags,
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
                    let prev_flags = store::flags_to_u8(msg.is_read, msg.is_starred);
                    let new_starred = !msg.is_starred;
                    msg.is_starred = new_starred;
                    let envelope_hash = msg.envelope_hash;
                    let mailbox_hash = msg.mailbox_hash;
                    let new_flags = store::flags_to_u8(msg.is_read, new_starred);
                    let pending_op = if new_starred { "set_flagged" } else { "unset_flagged" }.to_string();

                    let mut tasks: Vec<Task<Message>> = Vec::new();

                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        let Some(account_id) = self
                            .account_for_mailbox(mailbox_hash)
                            .and_then(|i| self.accounts.get(i))
                            .map(|a| a.config.id.clone())
                        else {
                            let err = format!(
                                "Cannot update cache flags: no account for mailbox {}",
                                mailbox_hash
                            );
                            log::error!("{}", err);
                            self.status_message = err;
                            return Task::none();
                        };
                        let op = pending_op.clone();
                        tasks.push(cosmic::task::future(async move {
                            if let Err(e) = cache
                                .update_flags(account_id, envelope_hash, new_flags, op)
                                .await
                            {
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
                                prev_flags,
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
                            if let Some(removed) = self.remove_message_optimistic(index) {
                                self.pending_move_restore
                                    .insert(envelope_hash, (removed, index));
                                return self.dispatch_move(envelope_hash, source_mailbox, trash_hash);
                            }
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
                            if let Some(removed) = self.remove_message_optimistic(index) {
                                self.pending_move_restore
                                    .insert(envelope_hash, (removed, index));
                                return self.dispatch_move(envelope_hash, source_mailbox, archive_hash);
                            }
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
                    if let Some(removed) = self.remove_message_optimistic(index) {
                        self.pending_move_restore
                            .insert(envelope_hash, (removed, index));
                        return self.dispatch_move(envelope_hash, source_mailbox, dest_mailbox);
                    }
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
                prev_flags,
                result,
            } => {
                match result {
                    Ok(new_flags) => {
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let Some(account_id) = self
                                .messages
                                .iter()
                                .find(|m| m.envelope_hash == envelope_hash)
                                .and_then(|m| self.account_for_mailbox(m.mailbox_hash))
                                .and_then(|i| self.accounts.get(i))
                                .map(|a| a.config.id.clone())
                            else {
                                let err = format!(
                                    "Cannot clear cache pending op: no account for message {}",
                                    envelope_hash
                                );
                                log::error!("{}", err);
                                self.status_message = err;
                                return Task::none();
                            };
                            return cosmic::task::future(async move {
                                if let Err(e) = cache
                                    .clear_pending_op(account_id, envelope_hash, new_flags)
                                    .await
                                {
                                    log::warn!("Failed to clear pending op: {}", e);
                                }
                                Message::Noop
                            });
                        }
                    }
                    Err(e) => {
                        log::error!("Flag operation failed: {}", e);
                        self.status_message = format!("Flag update failed: {}", e);

                        // Revert optimistic UI to exact pre-op flags.
                        if let Some(msg) = self.messages.iter_mut().find(|m| m.envelope_hash == envelope_hash) {
                            let (is_read, is_starred) = store::flags_from_u8(prev_flags);
                            msg.is_read = is_read;
                            msg.is_starred = is_starred;
                        }

                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let Some(account_id) = self
                                .messages
                                .iter()
                                .find(|m| m.envelope_hash == envelope_hash)
                                .and_then(|m| self.account_for_mailbox(m.mailbox_hash))
                                .and_then(|i| self.accounts.get(i))
                                .map(|a| a.config.id.clone())
                            else {
                                let err = format!(
                                    "Cannot revert cache pending op: no account for message {}",
                                    envelope_hash
                                );
                                log::error!("{}", err);
                                self.status_message = err;
                                return Task::none();
                            };
                            return cosmic::task::future(async move {
                                if let Err(e) =
                                    cache.revert_pending_op(account_id, envelope_hash).await
                                {
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
                        let Some(account_id) = self
                            .pending_move_restore
                            .get(&envelope_hash)
                            .and_then(|(msg, _)| self.account_for_mailbox(msg.mailbox_hash))
                            .and_then(|i| self.accounts.get(i))
                            .map(|a| a.config.id.clone())
                        else {
                            let err = format!(
                                "Cannot remove moved message from cache: missing account for {}",
                                envelope_hash
                            );
                            log::error!("{}", err);
                            self.status_message = err;
                            self.pending_move_restore.remove(&envelope_hash);
                            return Task::none();
                        };
                        self.pending_move_restore.remove(&envelope_hash);
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            return cosmic::task::future(async move {
                                if let Err(e) =
                                    cache.remove_message(account_id, envelope_hash).await
                                {
                                    log::warn!("Failed to remove message from cache: {}", e);
                                }
                                Message::Noop
                            });
                        }
                    }
                    Err(e) => {
                        if let Some((msg, original_index)) =
                            self.pending_move_restore.remove(&envelope_hash)
                        {
                            let insert_at = original_index.min(self.messages.len());
                            self.messages.insert(insert_at, msg);
                            self.selected_message = Some(insert_at);
                            self.recompute_visible();
                        }
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
    fn remove_message_optimistic(&mut self, index: usize) -> Option<neverlight_mail_core::models::MessageSummary> {
        if index >= self.messages.len() {
            return None;
        }

        let removed = self.messages.remove(index);
        match self.selected_message {
            Some(sel) if sel == index => {
                self.selected_message = if self.messages.is_empty() {
                    None
                } else {
                    Some(index.min(self.messages.len() - 1))
                };
                self.preview_body.clear();
                self.preview_markdown.clear();
                self.preview_attachments.clear();
                self.preview_image_handles.clear();
            }
            Some(sel) if sel > index => {
                self.selected_message = Some(sel - 1);
            }
            _ => {}
        }
        self.recompute_visible();
        Some(removed)
    }

    /// Dispatch IMAP move + cache update tasks for a message move operation.
    fn dispatch_move(
        &mut self,
        envelope_hash: u64,
        source_mailbox: u64,
        dest_mailbox: u64,
    ) -> Task<Message> {
        let mut tasks: Vec<Task<Message>> = Vec::new();

        if let Some(cache) = &self.cache {
            let cache = cache.clone();
            let Some(account_id) = self
                .account_for_mailbox(source_mailbox)
                .and_then(|i| self.accounts.get(i))
                .map(|a| a.config.id.clone())
            else {
                let err = format!(
                    "Cannot queue move cache update: no account for source mailbox {}",
                    source_mailbox
                );
                log::error!("{}", err);
                self.status_message = err;
                return Task::none();
            };
            let new_flags = store::flags_to_u8(true, false);
            tasks.push(cosmic::task::future(async move {
                if let Err(e) = cache
                    .update_flags(
                        account_id,
                        envelope_hash,
                        new_flags,
                        format!("move:{}", dest_mailbox),
                    )
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
