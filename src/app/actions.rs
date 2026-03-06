use cosmic::app::Task;
use neverlight_mail_core::store;
use neverlight_mail_core::{EnvelopeHash, Flag, FlagOp, MailboxHash};

use super::{
    ActionKind, AppModel, FlagIntentKind, MailboxIdentity, Message, MessageIdentity,
    PendingFlagIntent, PendingMoveIntent, Phase, RecoverableActionError, RetryAction,
};

fn error_indicates_dead_session(e: &str) -> bool {
    let lower = e.to_lowercase();
    lower.contains("broken pipe")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("not connected")
        || lower.contains("connection refused")
        || lower.contains("eof")
}

fn move_postcondition_retry_message(result: &Result<bool, String>) -> Option<String> {
    match result {
        Ok(true) => None,
        Ok(false) => Some(
            "Move completed but source TOC still contains the message (retryable). Reconciling..."
                .to_string(),
        ),
        Err(e) => Some(format!("Move postcondition check failed (retryable): {}", e)),
    }
}

fn queue_latest_intent<T>(
    in_flight: bool,
    pending: &mut Option<T>,
    incoming: T,
) -> Option<T> {
    if in_flight {
        *pending = Some(incoming);
        None
    } else {
        Some(incoming)
    }
}

impl AppModel {
    pub(super) fn handle_actions(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::AutoMarkRead(envelope_hash) => {
                if self.auto_read_suppressed {
                    return Task::none();
                }
                let still_selected = self
                    .selected_message
                    .and_then(|i| self.messages.get(i))
                    .is_some_and(|m| m.envelope_hash == envelope_hash && !m.is_read);
                if still_selected {
                    let index = self.selected_message.unwrap();
                    return self.dispatch(Message::ToggleRead(index));
                }
            }
            Message::ToggleRead(index) => {
                if let Some(msg) = self.messages.get(index) {
                    let Some(account_id) = self
                        .account_for_mailbox(msg.mailbox_hash)
                        .and_then(|i| self.accounts.get(i))
                        .map(|a| a.config.id.clone())
                    else {
                        self.status_message =
                            format!("Cannot resolve account for mailbox {}", msg.mailbox_hash);
                        return Task::none();
                    };
                    return self.queue_or_start_flag(PendingFlagIntent {
                        message: MessageIdentity {
                            account_id,
                            mailbox_hash: msg.mailbox_hash,
                            envelope_hash: msg.envelope_hash,
                        },
                        kind: FlagIntentKind::ToggleRead,
                    });
                }
            }
            Message::ToggleStar(index) => {
                if let Some(msg) = self.messages.get(index) {
                    let Some(account_id) = self
                        .account_for_mailbox(msg.mailbox_hash)
                        .and_then(|i| self.accounts.get(i))
                        .map(|a| a.config.id.clone())
                    else {
                        self.status_message =
                            format!("Cannot resolve account for mailbox {}", msg.mailbox_hash);
                        return Task::none();
                    };
                    return self.queue_or_start_flag(PendingFlagIntent {
                        message: MessageIdentity {
                            account_id,
                            mailbox_hash: msg.mailbox_hash,
                            envelope_hash: msg.envelope_hash,
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

                // Prevent cross-account moves (IMAP MOVE is intra-server only)
                if source.account_id != dest.account_id {
                    self.status_message = "Cannot move messages between accounts".into();
                    return Task::none();
                }
                if message.account_id != source.account_id
                    || message.mailbox_hash != source.mailbox_hash
                {
                    self.status_message = "Cannot move message: source identity mismatch".into();
                    return Task::none();
                }
                if !self.mailbox_belongs_to_account(&source.account_id, source.mailbox_hash)
                    || !self.mailbox_belongs_to_account(&dest.account_id, dest.mailbox_hash)
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
                self.flag_in_flight = false;

                let mut tasks: Vec<Task<Message>> = Vec::new();
                match result {
                    Ok(new_flags) => {
                        self.clear_error_surface();
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let account_id = message.account_id.clone();
                            let envelope_hash = message.envelope_hash;
                            tasks.push(cosmic::task::future(async move {
                                if let Err(e) = cache
                                    .clear_pending_op(account_id, envelope_hash, new_flags)
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
                            envelope_hash: Some(message.envelope_hash),
                            mailbox_hash: Some(message.mailbox_hash),
                        });
                        self.phase = Phase::Error;

                        // Revert optimistic UI to exact pre-op flags.
                        if let Some(msg) = self
                            .messages
                            .iter_mut()
                            .find(|m| {
                                m.envelope_hash == message.envelope_hash
                                    && m.mailbox_hash == message.mailbox_hash
                            })
                        {
                            let (is_read, is_starred) = store::flags_from_u8(prev_flags);
                            msg.is_read = is_read;
                            msg.is_starred = is_starred;
                        }

                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let account_id = message.account_id.clone();
                            let envelope_hash = message.envelope_hash;
                            tasks.push(cosmic::task::future(async move {
                                if let Err(e) =
                                    cache.revert_pending_op(account_id, envelope_hash).await
                                {
                                    log::warn!("Failed to revert pending op: {}", e);
                                }
                                Message::Noop
                            }));
                        }

                        // Dead session likely caused the failure — drop and reconnect
                        if let Some(idx) = self.account_index(&message.account_id) {
                            if self.accounts[idx].session.is_none()
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
                if let Some(next) = self.pending_flag_intent.take() {
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
                        let envelope_hash = message.envelope_hash;
                        self.pending_move_restore.remove(&message);
                        let mut tasks: Vec<Task<Message>> = Vec::new();
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let account_id_for_cache = account_id.clone();
                            tasks.push(cosmic::task::future(async move {
                                if let Err(e) = cache
                                    .remove_message(account_id_for_cache, envelope_hash)
                                    .await
                                {
                                    log::warn!("Failed to remove message from cache: {}", e);
                                }
                                Message::Noop
                            }));
                        }
                        if let Some(session) =
                            self.session_for_account_mailbox(&source.account_id, source.mailbox_hash)
                        {
                            let cache = self.cache.clone();
                            let account_id_for_check = account_id;
                            let message_for_check = message.clone();
                            let source_for_check = source.clone();
                            tasks.push(cosmic::task::future(async move {
                                let result = session
                                    .fetch_messages(MailboxHash(source_for_check.mailbox_hash))
                                    .await
                                    .map(|messages| {
                                        let contains = messages
                                            .iter()
                                            .any(|m| m.envelope_hash == envelope_hash);
                                        if let Some(cache) = cache {
                                            let account_id_for_save = account_id_for_check.clone();
                                            let messages_for_save = messages.clone();
                                            tokio::spawn(async move {
                                                if let Err(e) = cache
                                                    .save_messages(
                                                        account_id_for_save,
                                                        source_for_check.mailbox_hash,
                                                        messages_for_save,
                                                    )
                                                    .await
                                                {
                                                    log::warn!(
                                                        "Failed to cache reconciled source mailbox: {}",
                                                        e
                                                    );
                                                }
                                            });
                                        }
                                        !contains
                                    });
                                Message::MovePostconditionChecked {
                                    message: message_for_check,
                                    source: source_for_check,
                                    epoch,
                                    result,
                                }
                            }));
                        } else {
                            self.mutation_in_flight = false;
                            tasks.push(self.try_run_next_move_intent());
                        }
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
                            envelope_hash: Some(message.envelope_hash),
                            mailbox_hash: Some(source.mailbox_hash),
                        });
                        self.phase = Phase::Error;
                        self.mutation_in_flight = false;

                        // Dead session likely caused the failure — drop and reconnect
                        let mut tasks: Vec<Task<Message>> = Vec::new();
                        if let Some(idx) = self.account_index(&source.account_id) {
                            if self.accounts[idx].session.is_none()
                                || error_indicates_dead_session(&e)
                            {
                                tasks.push(self.drop_session_and_schedule_reconnect(
                                    idx,
                                    "move-failed",
                                ));
                            }
                        }
                        tasks.push(self.try_run_next_move_intent());
                        return cosmic::task::batch(tasks);
                    }
                }
            }
            Message::MovePostconditionChecked {
                message,
                source,
                epoch,
                result,
            } => {
                // Only apply if this completion corresponds to latest move epoch for this envelope.
                // If map entry is gone, the move already finalized; still allow same-epoch completion.
                if self
                    .pending_move_epochs
                    .get(&message)
                    .is_some_and(|latest| *latest != epoch)
                {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.mutation_in_flight = false;
                let mut tasks: Vec<Task<Message>> = Vec::new();
                if let Some(retry_message) = move_postcondition_retry_message(&result) {
                    self.postcondition_failure_count =
                        self.postcondition_failure_count.saturating_add(1);
                    if matches!(result, Ok(false)) {
                        self.toc_drift_count = self.toc_drift_count.saturating_add(1);
                    }
                    self.set_recoverable_action_error(RecoverableActionError {
                        action: ActionKind::Move,
                        message: retry_message,
                        retry: RetryAction::Refresh,
                        envelope_hash: Some(message.envelope_hash),
                        mailbox_hash: Some(source.mailbox_hash),
                    });
                    if let Err(e) = result {
                        log::warn!("Move postcondition check failed: {}", e);
                    }
                    tasks.push(self.dispatch(Message::Refresh));
                } else {
                    self.clear_error_surface();
                }
                if let Some(next) = self.pending_move_intent.take() {
                    tasks.push(self.dispatch(Message::RunMoveIntent(next)));
                }
                if tasks.is_empty() {
                    return Task::none();
                }
                return cosmic::task::batch(tasks);
            }
            _ => {}
        }
        Task::none()
    }

    fn try_run_next_flag_intent(&mut self) -> Task<Message> {
        if let Some(next) = self.pending_flag_intent.take() {
            return self.dispatch(Message::RunFlagIntent(next));
        }
        Task::none()
    }

    fn try_run_next_move_intent(&mut self) -> Task<Message> {
        if let Some(next) = self.pending_move_intent.take() {
            return self.dispatch(Message::RunMoveIntent(next));
        }
        Task::none()
    }

    fn queue_or_start_flag(&mut self, intent: PendingFlagIntent) -> Task<Message> {
        if let Some(start_now) =
            queue_latest_intent(self.flag_in_flight, &mut self.pending_flag_intent, intent)
        {
            return self.dispatch(Message::RunFlagIntent(start_now));
        }
        self.status_message = "Flag update queued...".into();
        Task::none()
    }

    fn run_flag_intent(&mut self, intent: PendingFlagIntent) -> Task<Message> {
        let message_id = intent.message.clone();
        let Some(index) = self
            .messages
            .iter()
            .position(|m| {
                m.envelope_hash == message_id.envelope_hash
                    && m.mailbox_hash == message_id.mailbox_hash
            })
        else {
            return self.try_run_next_flag_intent();
        };

        let Some(msg) = self.messages.get_mut(index) else {
            return self.try_run_next_flag_intent();
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
                        FlagOp::Set(Flag::SEEN)
                    } else {
                        FlagOp::UnSet(Flag::SEEN)
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
                        FlagOp::Set(Flag::FLAGGED)
                    } else {
                        FlagOp::UnSet(Flag::FLAGGED)
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
            let envelope_hash = message_id.envelope_hash;
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

        if let Some(session) =
            self.session_for_account_mailbox(&message_id.account_id, message_id.mailbox_hash)
        {
            self.flag_epoch = self.flag_epoch.saturating_add(1);
            let epoch = self.flag_epoch;
            self.pending_flag_epochs.insert(message_id.clone(), epoch);
            self.flag_in_flight = true;
            op_epoch = Some(epoch);
            let message_for_completion = message_id.clone();
            tasks.push(cosmic::task::future(async move {
                let result = session
                    .set_flags(
                        EnvelopeHash(message_for_completion.envelope_hash),
                        MailboxHash(message_for_completion.mailbox_hash),
                        vec![flag_op],
                    )
                    .await;
                Message::FlagOpComplete {
                    message: message_for_completion,
                    epoch,
                    prev_flags,
                    result: result.map(|_| new_flags),
                }
            }));
        }

        if op_epoch.is_none() {
            self.pending_flag_epochs.remove(&message_id);
            self.flag_in_flight = false;
        }
        if tasks.is_empty() {
            Task::none()
        } else {
            cosmic::task::batch(tasks)
        }
    }

    fn queue_or_start_move(&mut self, intent: PendingMoveIntent) -> Task<Message> {
        if let Some(start_now) = queue_latest_intent(
            self.mutation_in_flight,
            &mut self.pending_move_intent,
            intent,
        ) {
            return self.dispatch(Message::RunMoveIntent(start_now));
        }
        self.status_message = "Move queued...".into();
        Task::none()
    }

    fn run_move_intent(&mut self, intent: PendingMoveIntent) -> Task<Message> {
        if intent.source == intent.dest {
            return self.try_run_next_move_intent();
        }
        if self
            .session_for_account_mailbox(&intent.source.account_id, intent.source.mailbox_hash)
            .is_none()
        {
            self.status_message = "Move failed: account is offline".into();
            return self.try_run_next_move_intent();
        }
        let Some(index) = self.messages.iter().position(|m| {
            m.envelope_hash == intent.message.envelope_hash
                && m.mailbox_hash == intent.source.mailbox_hash
        }) else {
            return self.try_run_next_move_intent();
        };
        if let Some(removed) = self.remove_message_optimistic(index) {
            self.pending_move_restore
                .insert(intent.message.clone(), (removed, index));
            return self.dispatch_move(intent.message, intent.source, intent.dest);
        }
        self.try_run_next_move_intent()
    }

    fn trash_intent_for_index(&mut self, index: usize) -> Option<PendingMoveIntent> {
        if let Some(msg) = self.messages.get(index) {
            let mailbox_hash = msg.mailbox_hash;
            let account_id = self
                .account_for_mailbox(mailbox_hash)
                .and_then(|i| self.accounts.get(i))
                .map(|a| a.config.id.clone())?;
            if let Some(folder_map) =
                self.folder_map_for_account_mailbox(&account_id, mailbox_hash)
            {
                if let Some(trash_hash) = folder_map
                    .get("Trash")
                    .or_else(|| folder_map.get("INBOX.Trash"))
                    .copied()
                {
                    return Some(PendingMoveIntent {
                        message: MessageIdentity {
                            account_id: account_id.clone(),
                            mailbox_hash: msg.mailbox_hash,
                            envelope_hash: msg.envelope_hash,
                        },
                        source: MailboxIdentity {
                            account_id: account_id.clone(),
                            mailbox_hash: msg.mailbox_hash,
                        },
                        dest: MailboxIdentity {
                            account_id,
                            mailbox_hash: trash_hash,
                        },
                    });
                }
            }
            self.status_message = "Trash folder not found".into();
        }
        None
    }

    fn archive_intent_for_index(&mut self, index: usize) -> Option<PendingMoveIntent> {
        if let Some(msg) = self.messages.get(index) {
            let mailbox_hash = msg.mailbox_hash;
            let account_id = self
                .account_for_mailbox(mailbox_hash)
                .and_then(|i| self.accounts.get(i))
                .map(|a| a.config.id.clone())?;
            if let Some(folder_map) =
                self.folder_map_for_account_mailbox(&account_id, mailbox_hash)
            {
                if let Some(archive_hash) = folder_map
                    .get("Archive")
                    .or_else(|| folder_map.get("INBOX.Archive"))
                    .copied()
                {
                    return Some(PendingMoveIntent {
                        message: MessageIdentity {
                            account_id: account_id.clone(),
                            mailbox_hash: msg.mailbox_hash,
                            envelope_hash: msg.envelope_hash,
                        },
                        source: MailboxIdentity {
                            account_id: account_id.clone(),
                            mailbox_hash: msg.mailbox_hash,
                        },
                        dest: MailboxIdentity {
                            account_id,
                            mailbox_hash: archive_hash,
                        },
                    });
                }
            }
            self.status_message = "Archive folder not found".into();
        }
        None
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

    /// Dispatch IMAP move + cache update tasks for a message move operation.
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
            let envelope_hash = message.envelope_hash;
            let dest_mailbox = dest.mailbox_hash;
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

        if let Some(session) =
            self.session_for_account_mailbox(&source.account_id, source.mailbox_hash)
        {
            self.mutation_in_flight = true;
            self.mutation_epoch = self.mutation_epoch.saturating_add(1);
            let epoch = self.mutation_epoch;
            self.pending_move_epochs.insert(message.clone(), epoch);
            let message_for_completion = message.clone();
            let source_for_completion = source.clone();
            tasks.push(cosmic::task::future(async move {
                let result = session
                    .move_messages(
                        EnvelopeHash(message_for_completion.envelope_hash),
                        MailboxHash(source_for_completion.mailbox_hash),
                        MailboxHash(dest.mailbox_hash),
                    )
                    .await;
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

#[cfg(test)]
mod tests {
    use super::{move_postcondition_retry_message, queue_latest_intent};
    use crate::app::{
        FlagIntentKind, MailboxIdentity, MessageIdentity, PendingFlagIntent, PendingMoveIntent,
    };

    #[test]
    fn move_postcondition_ok_true_is_noop() {
        assert_eq!(move_postcondition_retry_message(&Ok(true)), None);
    }

    #[test]
    fn move_postcondition_ok_false_requires_retryable_reconcile() {
        let msg = move_postcondition_retry_message(&Ok(false)).expect("message");
        assert!(msg.contains("retryable"));
        assert!(msg.contains("Reconciling"));
    }

    #[test]
    fn move_postcondition_err_requires_retryable_reconcile() {
        let msg =
            move_postcondition_retry_message(&Err("imap timeout".to_string())).expect("message");
        assert!(msg.contains("retryable"));
        assert!(msg.contains("imap timeout"));
    }

    #[test]
    fn flag_lane_queue_keeps_only_latest_pending_intent() {
        let mut pending: Option<PendingFlagIntent> = None;
        let first = PendingFlagIntent {
            message: MessageIdentity {
                account_id: "a1".into(),
                mailbox_hash: 10,
                envelope_hash: 1,
            },
            kind: FlagIntentKind::ToggleRead,
        };
        let second = PendingFlagIntent {
            message: MessageIdentity {
                account_id: "a1".into(),
                mailbox_hash: 10,
                envelope_hash: 2,
            },
            kind: FlagIntentKind::ToggleStar,
        };

        assert_eq!(
            queue_latest_intent(false, &mut pending, first.clone()),
            Some(first.clone())
        );
        assert_eq!(pending, None);
        assert_eq!(queue_latest_intent(true, &mut pending, first.clone()), None);
        assert_eq!(pending, Some(first));
        assert_eq!(queue_latest_intent(true, &mut pending, second.clone()), None);
        assert_eq!(pending, Some(second));
    }

    #[test]
    fn mutation_lane_queue_keeps_only_latest_pending_intent() {
        let mut pending: Option<PendingMoveIntent> = None;
        let first = PendingMoveIntent {
            message: MessageIdentity {
                account_id: "a1".into(),
                mailbox_hash: 11,
                envelope_hash: 7,
            },
            source: MailboxIdentity {
                account_id: "a1".into(),
                mailbox_hash: 11,
            },
            dest: MailboxIdentity {
                account_id: "a1".into(),
                mailbox_hash: 22,
            },
        };
        let second = PendingMoveIntent {
            message: MessageIdentity {
                account_id: "a1".into(),
                mailbox_hash: 11,
                envelope_hash: 8,
            },
            source: MailboxIdentity {
                account_id: "a1".into(),
                mailbox_hash: 11,
            },
            dest: MailboxIdentity {
                account_id: "a1".into(),
                mailbox_hash: 33,
            },
        };

        assert_eq!(
            queue_latest_intent(false, &mut pending, first.clone()),
            Some(first.clone())
        );
        assert_eq!(pending, None);
        assert_eq!(queue_latest_intent(true, &mut pending, first.clone()), None);
        assert_eq!(pending, Some(first));
        assert_eq!(queue_latest_intent(true, &mut pending, second.clone()), None);
        assert_eq!(pending, Some(second));
    }
}
