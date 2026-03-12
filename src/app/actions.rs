use cosmic::app::Task;
use neverlight_mail_core::store;
use neverlight_mail_core::FlagOp;

use super::{
    ActionKind, AppModel, FlagIntentKind, MailboxIdentity, Message, MessageIdentity,
    PendingFlagIntent, PendingMoveIntent, Phase, RecoverableActionError, RetryAction,
};

fn error_indicates_dead_session(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("connection refused")
        || lower.contains("timeout")
}

impl AppModel {
    pub(super) fn handle_actions(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::AutoMarkRead(email_id) => {
                if self.auto_read_suppressed {
                    return Task::none();
                }
                let still_selected = self
                    .selected_message
                    .and_then(|i| self.messages.get(i))
                    .is_some_and(|m| m.email_id == email_id && !m.is_read);
                if let (true, Some(index)) = (still_selected, self.selected_message) {
                    return self.dispatch(Message::ToggleRead(index));
                }
            }
            Message::ToggleRead(index) => {
                if let Some(msg) = self.messages.get(index) {
                    return self.queue_or_start_flag(PendingFlagIntent {
                        message: MessageIdentity {
                            account_id: msg.account_id.clone(),
                            mailbox_id: msg.mailbox_id.clone(),
                            email_id: msg.email_id.clone(),
                        },
                        kind: FlagIntentKind::ToggleRead,
                    });
                }
            }
            Message::ToggleStar(index) => {
                if let Some(msg) = self.messages.get(index) {
                    return self.queue_or_start_flag(PendingFlagIntent {
                        message: MessageIdentity {
                            account_id: msg.account_id.clone(),
                            mailbox_id: msg.mailbox_id.clone(),
                            email_id: msg.email_id.clone(),
                        },
                        kind: FlagIntentKind::ToggleStar,
                    });
                }
            }
            Message::Delete(index) => {
                return self.dispatch(Message::Trash(index));
            }
            Message::RunFlagIntent(intent) => {
                return self.run_flag_intent(intent);
            }
            Message::Trash(index) => {
                if let Some(intent) = self.trash_intent_for_index(index) {
                    return self.queue_or_start_move(intent);
                }
            }
            Message::Archive(index) => {
                if let Some(intent) = self.archive_intent_for_index(index) {
                    return self.queue_or_start_move(intent);
                }
            }
            Message::RunMoveIntent(intent) => {
                return self.run_move_intent(intent);
            }
            Message::DragMessageToFolder {
                message,
                source,
                dest,
            } => {
                self.folder_drag_target = None;

                // No-op if dragged onto the same folder
                if source == dest {
                    return Task::none();
                }

                // Prevent cross-account moves
                if source.account_id != dest.account_id {
                    self.status_message = "Cannot move messages between accounts".into();
                    return Task::none();
                }
                if message.account_id != source.account_id
                    || message.mailbox_id != source.mailbox_id
                {
                    self.status_message = "Cannot move message: source identity mismatch".into();
                    return Task::none();
                }
                if !self.mailbox_belongs_to_account(&source.account_id, &source.mailbox_id)
                    || !self.mailbox_belongs_to_account(&dest.account_id, &dest.mailbox_id)
                {
                    self.status_message = "Cannot move message: mailbox no longer available".into();
                    return Task::none();
                }

                return self.queue_or_start_move(PendingMoveIntent {
                    message,
                    source,
                    dest,
                });
            }
            Message::FolderDragEnter(i) => {
                self.folder_drag_target = Some(i);
            }
            Message::FolderDragLeave => {
                self.folder_drag_target = None;
            }
            Message::FlagOpComplete {
                message,
                epoch,
                prev_flags,
                result,
            } => {
                if self.pending_flag_epochs.get(&message).copied() != Some(epoch) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.pending_flag_epochs.remove(&message);
                self.flag_in_flight_accounts.remove(&message.account_id);

                let mut tasks: Vec<Task<Message>> = Vec::new();
                match result {
                    Ok(new_flags) => {
                        self.clear_error_surface();
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let account_id = message.account_id.clone();
                            let email_id = message.email_id.clone();
                            tasks.push(cosmic::task::future(async move {
                                if let Err(e) = cache
                                    .clear_pending_op(account_id, email_id, new_flags)
                                    .await
                                {
                                    log::warn!("Failed to clear pending op: {}", e);
                                }
                                Message::Noop
                            }));
                        }
                    }
                    Err(e) => {
                        log::error!("Flag operation failed: {}", e);
                        self.set_recoverable_action_error(RecoverableActionError {
                            action: ActionKind::Flag,
                            message: format!("Flag update failed: {}", e),
                            retry: RetryAction::Refresh,
                            email_id: Some(message.email_id.clone()),
                            mailbox_id: Some(message.mailbox_id.clone()),
                        });
                        self.phase = Phase::Error;

                        // Revert optimistic UI to exact pre-op flags.
                        if let Some(msg) = self
                            .messages
                            .iter_mut()
                            .find(|m| {
                                m.email_id == message.email_id
                                    && m.mailbox_id == message.mailbox_id
                            })
                        {
                            let (is_read, is_starred) = store::flags_from_u8(prev_flags);
                            msg.is_read = is_read;
                            msg.is_starred = is_starred;
                        }

                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let account_id = message.account_id.clone();
                            let email_id = message.email_id.clone();
                            tasks.push(cosmic::task::future(async move {
                                if let Err(e) =
                                    cache.revert_pending_op(account_id, email_id).await
                                {
                                    log::warn!("Failed to revert pending op: {}", e);
                                }
                                Message::Noop
                            }));
                        }

                        // Dead session likely caused the failure — drop and reconnect
                        if let Some(idx) = self.account_index(&message.account_id) {
                            if self.accounts[idx].client.is_none()
                                || error_indicates_dead_session(&e)
                            {
                                tasks.push(
                                    self.drop_session_and_schedule_reconnect(
                                        idx,
                                        "flag-failed",
                                    ),
                                );
                            }
                        }
                    }
                }
                if let Some(next) = self.pending_flag_intents.remove(&message.account_id) {
                    tasks.push(self.dispatch(Message::RunFlagIntent(next)));
                }
                if tasks.is_empty() {
                    return Task::none();
                }
                return cosmic::task::batch(tasks);
            }
            Message::MoveOpComplete {
                message,
                source,
                epoch,
                result,
            } => {
                if self.pending_move_epochs.get(&message).copied() != Some(epoch) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.pending_move_epochs.remove(&message);
                match result {
                    Ok(()) => {
                        self.clear_error_surface();
                        let account_id = message.account_id.clone();
                        let email_id = message.email_id.clone();
                        self.pending_move_restore.remove(&message);
                        let mut tasks: Vec<Task<Message>> = Vec::new();
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let account_id_for_cache = account_id.clone();
                            let email_id_for_cache = email_id.clone();
                            tasks.push(cosmic::task::future(async move {
                                if let Err(e) = cache
                                    .remove_message(account_id_for_cache, email_id_for_cache)
                                    .await
                                {
                                    log::warn!("Failed to remove message from cache: {}", e);
                                }
                                Message::Noop
                            }));
                        }

                        // JMAP moves are atomic (Email/set with mailboxIds patch).
                        // No postcondition verification needed — the server either
                        // succeeds or returns an error. Emit MovePostconditionChecked
                        // with Ok(true) directly to keep the message flow consistent.
                        self.mutation_in_flight_accounts.remove(&source.account_id);
                        tasks.push(self.try_run_next_move_intent_for(&source.account_id));

                        if !tasks.is_empty() {
                            return cosmic::task::batch(tasks);
                        }
                    }
                    Err(e) => {
                        if let Some((msg, original_index)) =
                            self.pending_move_restore.remove(&message)
                        {
                            let insert_at = original_index.min(self.messages.len());
                            self.messages.insert(insert_at, msg);
                            self.selected_message = Some(insert_at);
                            self.recompute_visible();
                        }
                        log::error!("Move operation failed: {}", e);
                        self.set_recoverable_action_error(RecoverableActionError {
                            action: ActionKind::Move,
                            message: format!("Move failed: {}", e),
                            retry: RetryAction::Refresh,
                            email_id: Some(message.email_id.clone()),
                            mailbox_id: Some(source.mailbox_id.clone()),
                        });
                        self.phase = Phase::Error;
                        self.mutation_in_flight_accounts.remove(&source.account_id);

                        // Dead session likely caused the failure — drop and reconnect
                        let mut tasks: Vec<Task<Message>> = Vec::new();
                        if let Some(idx) = self.account_index(&source.account_id) {
                            if self.accounts[idx].client.is_none()
                                || error_indicates_dead_session(&e)
                            {
                                tasks.push(self.drop_session_and_schedule_reconnect(
                                    idx,
                                    "move-failed",
                                ));
                            }
                        }
                        tasks.push(self.try_run_next_move_intent_for(&source.account_id));
                        return cosmic::task::batch(tasks);
                    }
                }
            }
            _ => {}
        }
        Task::none()
    }

    fn try_run_next_flag_intent_for(&mut self, account_id: &str) -> Task<Message> {
        if let Some(next) = self.pending_flag_intents.remove(account_id) {
            return self.dispatch(Message::RunFlagIntent(next));
        }
        Task::none()
    }

    fn try_run_next_move_intent_for(&mut self, account_id: &str) -> Task<Message> {
        if let Some(next) = self.pending_move_intents.remove(account_id) {
            return self.dispatch(Message::RunMoveIntent(next));
        }
        Task::none()
    }

    fn queue_or_start_flag(&mut self, intent: PendingFlagIntent) -> Task<Message> {
        let account_id = intent.message.account_id.clone();
        if self.flag_in_flight_accounts.contains(&account_id) {
            self.pending_flag_intents.insert(account_id, intent);
            self.status_message = "Flag update queued...".into();
            return Task::none();
        }
        self.dispatch(Message::RunFlagIntent(intent))
    }

    fn run_flag_intent(&mut self, intent: PendingFlagIntent) -> Task<Message> {
        let message_id = intent.message.clone();
        let Some(index) = self
            .messages
            .iter()
            .position(|m| {
                m.email_id == message_id.email_id
                    && m.mailbox_id == message_id.mailbox_id
            })
        else {
            return self.try_run_next_flag_intent_for(&message_id.account_id);
        };

        let Some(msg) = self.messages.get_mut(index) else {
            return self.try_run_next_flag_intent_for(&message_id.account_id);
        };

        let prev_flags = store::flags_to_u8(msg.is_read, msg.is_starred);
        let (new_read, new_starred, pending_op, flag_op) = match intent.kind {
            FlagIntentKind::ToggleRead => {
                let new_read = !msg.is_read;
                if !new_read {
                    self.auto_read_suppressed = true;
                }
                (
                    new_read,
                    msg.is_starred,
                    if new_read { "set_seen" } else { "unset_seen" }.to_string(),
                    if new_read {
                        FlagOp::SetSeen(true)
                    } else {
                        FlagOp::SetSeen(false)
                    },
                )
            }
            FlagIntentKind::ToggleStar => {
                let new_starred = !msg.is_starred;
                (
                    msg.is_read,
                    new_starred,
                    if new_starred {
                        "set_flagged"
                    } else {
                        "unset_flagged"
                    }
                    .to_string(),
                    if new_starred {
                        FlagOp::SetFlagged(true)
                    } else {
                        FlagOp::SetFlagged(false)
                    },
                )
            }
        };
        msg.is_read = new_read;
        msg.is_starred = new_starred;

        let new_flags = store::flags_to_u8(new_read, new_starred);
        let mut tasks: Vec<Task<Message>> = Vec::new();
        let mut op_epoch: Option<u64> = None;

        if let Some(cache) = &self.cache {
            let cache = cache.clone();
            let account_id = message_id.account_id.clone();
            let email_id = message_id.email_id.clone();
            let op = pending_op.clone();
            tasks.push(cosmic::task::future(async move {
                if let Err(e) = cache
                    .update_flags(account_id, email_id, new_flags, op)
                    .await
                {
                    log::warn!("Failed to update cache flags: {}", e);
                }
                Message::Noop
            }));
        }

        if let Some(client) = self.client_for_account(&message_id.account_id) {
            self.flag_epoch = self.flag_epoch.saturating_add(1);
            let epoch = self.flag_epoch;
            self.pending_flag_epochs.insert(message_id.clone(), epoch);
            self.flag_in_flight_accounts
                .insert(message_id.account_id.clone());
            op_epoch = Some(epoch);
            let message_for_completion = message_id.clone();
            let email_id = message_for_completion.email_id.clone();
            tasks.push(cosmic::task::future(async move {
                let result = neverlight_mail_core::email::set_flag(
                    &client,
                    &email_id,
                    &flag_op,
                )
                .await;
                Message::FlagOpComplete {
                    message: message_for_completion,
                    epoch,
                    prev_flags,
                    result: result.map(|_| new_flags).map_err(|e| e.to_string()),
                }
            }));
        }

        if op_epoch.is_none() {
            self.pending_flag_epochs.remove(&message_id);
            self.flag_in_flight_accounts.remove(&message_id.account_id);
        }
        if tasks.is_empty() {
            Task::none()
        } else {
            cosmic::task::batch(tasks)
        }
    }

    fn queue_or_start_move(&mut self, intent: PendingMoveIntent) -> Task<Message> {
        let account_id = intent.source.account_id.clone();
        if self.mutation_in_flight_accounts.contains(&account_id) {
            self.pending_move_intents.insert(account_id, intent);
            self.status_message = "Move queued...".into();
            return Task::none();
        }
        self.dispatch(Message::RunMoveIntent(intent))
    }

    fn run_move_intent(&mut self, intent: PendingMoveIntent) -> Task<Message> {
        let source_account_id = intent.source.account_id.clone();
        if intent.source == intent.dest {
            return self.try_run_next_move_intent_for(&source_account_id);
        }
        if self
            .client_for_account(&intent.source.account_id)
            .is_none()
        {
            self.status_message = "Move failed: account is offline".into();
            return self.try_run_next_move_intent_for(&source_account_id);
        }
        let Some(index) = self.messages.iter().position(|m| {
            m.email_id == intent.message.email_id
                && m.mailbox_id == intent.source.mailbox_id
        }) else {
            return self.try_run_next_move_intent_for(&source_account_id);
        };
        if let Some(removed) = self.remove_message_optimistic(index) {
            self.pending_move_restore
                .insert(intent.message.clone(), (removed, index));
            return self.dispatch_move(intent.message, intent.source, intent.dest);
        }
        self.try_run_next_move_intent_for(&source_account_id)
    }

    fn trash_intent_for_index(&mut self, index: usize) -> Option<PendingMoveIntent> {
        let msg = self.messages.get(index)?;
        let account_id = msg.account_id.clone();
        let mailbox_id = msg.mailbox_id.clone();
        let email_id = msg.email_id.clone();
        let acct = self
            .account_index(&account_id)
            .and_then(|idx| self.accounts.get(idx))?;
        let Some(trash_id) =
            neverlight_mail_core::mailbox::find_by_role(&acct.folders, "trash")
        else {
            self.status_message = "Trash folder not found".into();
            return None;
        };
        Some(PendingMoveIntent {
            message: MessageIdentity {
                account_id: account_id.clone(),
                mailbox_id: mailbox_id.clone(),
                email_id,
            },
            source: MailboxIdentity {
                account_id: account_id.clone(),
                mailbox_id,
            },
            dest: MailboxIdentity {
                account_id,
                mailbox_id: trash_id,
            },
        })
    }

    fn archive_intent_for_index(&mut self, index: usize) -> Option<PendingMoveIntent> {
        let msg = self.messages.get(index)?;
        let account_id = msg.account_id.clone();
        let mailbox_id = msg.mailbox_id.clone();
        let email_id = msg.email_id.clone();
        let acct = self
            .account_index(&account_id)
            .and_then(|idx| self.accounts.get(idx))?;
        let Some(archive_id) =
            neverlight_mail_core::mailbox::find_by_role(&acct.folders, "archive")
        else {
            self.status_message = "Archive folder not found".into();
            return None;
        };
        Some(PendingMoveIntent {
            message: MessageIdentity {
                account_id: account_id.clone(),
                mailbox_id: mailbox_id.clone(),
                email_id,
            },
            source: MailboxIdentity {
                account_id: account_id.clone(),
                mailbox_id,
            },
            dest: MailboxIdentity {
                account_id,
                mailbox_id: archive_id,
            },
        })
    }

    /// Optimistically remove a message from the list and adjust selection.
    pub(super) fn remove_message_optimistic(
        &mut self,
        index: usize,
    ) -> Option<neverlight_mail_core::models::MessageSummary> {
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

    /// Dispatch JMAP move + cache update tasks for a message move operation.
    fn dispatch_move(
        &mut self,
        message: MessageIdentity,
        source: MailboxIdentity,
        dest: MailboxIdentity,
    ) -> Task<Message> {
        let mut tasks: Vec<Task<Message>> = Vec::new();

        if let Some(cache) = &self.cache {
            let cache = cache.clone();
            let new_flags = store::flags_to_u8(true, false);
            let account_id = source.account_id.clone();
            let email_id = message.email_id.clone();
            let dest_mailbox_id = dest.mailbox_id.clone();
            tasks.push(cosmic::task::future(async move {
                if let Err(e) = cache
                    .update_flags(
                        account_id,
                        email_id,
                        new_flags,
                        format!("move:{}", dest_mailbox_id),
                    )
                    .await
                {
                    log::warn!("Failed to update cache for move: {}", e);
                }
                Message::Noop
            }));
        }

        if let Some(client) = self.client_for_account(&source.account_id) {
            self.mutation_in_flight_accounts
                .insert(source.account_id.clone());
            self.mutation_epoch = self.mutation_epoch.saturating_add(1);
            let epoch = self.mutation_epoch;
            self.pending_move_epochs.insert(message.clone(), epoch);
            let message_for_completion = message.clone();
            let source_for_completion = source.clone();
            let email_id = message.email_id.clone();
            let source_mailbox_id = source.mailbox_id.clone();
            let dest_mailbox_id = dest.mailbox_id.clone();
            tasks.push(cosmic::task::future(async move {
                let result = neverlight_mail_core::email::move_to(
                    &client,
                    &email_id,
                    &source_mailbox_id,
                    &dest_mailbox_id,
                )
                .await
                .map_err(|e| e.to_string());
                Message::MoveOpComplete {
                    message: message_for_completion,
                    source: source_for_completion,
                    epoch,
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

