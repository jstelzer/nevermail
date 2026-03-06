use cosmic::app::Task;
use neverlight_mail_core::store;
use neverlight_mail_core::{EnvelopeHash, Flag, FlagOp, MailboxHash};

use super::{
    ActionKind, AppModel, FlagIntentKind, Message, PendingFlagIntent, PendingMoveIntent,
    Phase, RecoverableActionError, RetryAction,
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

fn queue_latest_intent<T: Copy>(
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
                    return self.queue_or_start_flag(PendingFlagIntent {
                        envelope_hash: msg.envelope_hash,
                        mailbox_hash: msg.mailbox_hash,
                        kind: FlagIntentKind::ToggleRead,
                    });
                }
            }
            Message::ToggleStar(index) => {
                if let Some(msg) = self.messages.get(index) {
                    return self.queue_or_start_flag(PendingFlagIntent {
                        envelope_hash: msg.envelope_hash,
                        mailbox_hash: msg.mailbox_hash,
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

                return self.queue_or_start_move(PendingMoveIntent {
                    envelope_hash,
                    source_mailbox,
                    dest_mailbox,
                });
            }
            Message::FolderDragEnter(i) => {
                self.folder_drag_target = Some(i);
            }
            Message::FolderDragLeave => {
                self.folder_drag_target = None;
            }
            Message::FlagOpComplete {
                envelope_hash,
                epoch,
                prev_flags,
                result,
            } => {
                if self.pending_flag_epochs.get(&envelope_hash).copied() != Some(epoch) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.pending_flag_epochs.remove(&envelope_hash);
                self.flag_in_flight = false;

                let mut tasks: Vec<Task<Message>> = Vec::new();
                match result {
                    Ok(new_flags) => {
                        self.clear_error_surface();
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
                                return self.try_run_next_flag_intent();
                            };
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
                        let mailbox_hash = self
                            .messages
                            .iter()
                            .find(|m| m.envelope_hash == envelope_hash)
                            .map(|m| m.mailbox_hash);
                        self.set_recoverable_action_error(RecoverableActionError {
                            action: ActionKind::Flag,
                            message: format!("Flag update failed: {}", e),
                            retry: RetryAction::Refresh,
                            envelope_hash: Some(envelope_hash),
                            mailbox_hash,
                        });
                        self.phase = Phase::Error;

                        // Revert optimistic UI to exact pre-op flags.
                        if let Some(msg) = self
                            .messages
                            .iter_mut()
                            .find(|m| m.envelope_hash == envelope_hash)
                        {
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
                                return self.try_run_next_flag_intent();
                            };
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
                        if let Some(mh) = mailbox_hash {
                            if let Some(idx) = self.account_for_mailbox(mh) {
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
                envelope_hash,
                source_mailbox,
                epoch,
                result,
            } => {
                if self.pending_move_epochs.get(&envelope_hash).copied() != Some(epoch) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.pending_move_epochs.remove(&envelope_hash);
                match result {
                    Ok(()) => {
                        self.clear_error_surface();
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
                            self.mutation_in_flight = false;
                            return self.try_run_next_move_intent();
                        };
                        self.pending_move_restore.remove(&envelope_hash);
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
                        if let Some(session) = self.session_for_mailbox(source_mailbox) {
                            let cache = self.cache.clone();
                            let account_id_for_check = account_id;
                            tasks.push(cosmic::task::future(async move {
                                let result = session
                                    .fetch_messages(MailboxHash(source_mailbox))
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
                                                        source_mailbox,
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
                                    envelope_hash,
                                    source_mailbox,
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
                            self.pending_move_restore.remove(&envelope_hash)
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
                            envelope_hash: Some(envelope_hash),
                            mailbox_hash: Some(source_mailbox),
                        });
                        self.phase = Phase::Error;
                        self.mutation_in_flight = false;

                        // Dead session likely caused the failure — drop and reconnect
                        let mut tasks: Vec<Task<Message>> = Vec::new();
                        if let Some(idx) = self.account_for_mailbox(source_mailbox) {
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
                envelope_hash,
                source_mailbox: _source_mailbox,
                epoch,
                result,
            } => {
                // Only apply if this completion corresponds to latest move epoch for this envelope.
                // If map entry is gone, the move already finalized; still allow same-epoch completion.
                if self
                    .pending_move_epochs
                    .get(&envelope_hash)
                    .is_some_and(|latest| *latest != epoch)
                {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.mutation_in_flight = false;
                let mut tasks: Vec<Task<Message>> = Vec::new();
                if let Some(message) = move_postcondition_retry_message(&result) {
                    self.postcondition_failure_count =
                        self.postcondition_failure_count.saturating_add(1);
                    if matches!(result, Ok(false)) {
                        self.toc_drift_count = self.toc_drift_count.saturating_add(1);
                    }
                    self.set_recoverable_action_error(RecoverableActionError {
                        action: ActionKind::Move,
                        message,
                        retry: RetryAction::Refresh,
                        envelope_hash: Some(envelope_hash),
                        mailbox_hash: Some(_source_mailbox),
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
        let Some(index) = self
            .messages
            .iter()
            .position(|m| m.envelope_hash == intent.envelope_hash)
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
            let Some(account_id) = self
                .account_for_mailbox(intent.mailbox_hash)
                .and_then(|i| self.accounts.get(i))
                .map(|a| a.config.id.clone())
            else {
                let err = format!(
                    "Cannot update cache flags: no account for mailbox {}",
                    intent.mailbox_hash
                );
                log::error!("{}", err);
                self.status_message = err;
                return Task::none();
            };
            let op = pending_op.clone();
            tasks.push(cosmic::task::future(async move {
                if let Err(e) = cache
                    .update_flags(account_id, intent.envelope_hash, new_flags, op)
                    .await
                {
                    log::warn!("Failed to update cache flags: {}", e);
                }
                Message::Noop
            }));
        }

        if let Some(session) = self.session_for_mailbox(intent.mailbox_hash) {
            self.flag_epoch = self.flag_epoch.saturating_add(1);
            let epoch = self.flag_epoch;
            self.pending_flag_epochs.insert(intent.envelope_hash, epoch);
            self.flag_in_flight = true;
            op_epoch = Some(epoch);
            tasks.push(cosmic::task::future(async move {
                let result = session
                    .set_flags(
                        EnvelopeHash(intent.envelope_hash),
                        MailboxHash(intent.mailbox_hash),
                        vec![flag_op],
                    )
                    .await;
                Message::FlagOpComplete {
                    envelope_hash: intent.envelope_hash,
                    epoch,
                    prev_flags,
                    result: result.map(|_| new_flags),
                }
            }));
        }

        if op_epoch.is_none() {
            self.pending_flag_epochs.remove(&intent.envelope_hash);
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
        if intent.source_mailbox == intent.dest_mailbox {
            return self.try_run_next_move_intent();
        }
        if self.session_for_mailbox(intent.source_mailbox).is_none() {
            self.status_message = "Move failed: account is offline".into();
            return self.try_run_next_move_intent();
        }
        let Some(index) = self.messages.iter().position(|m| {
            m.envelope_hash == intent.envelope_hash && m.mailbox_hash == intent.source_mailbox
        }) else {
            return self.try_run_next_move_intent();
        };
        if let Some(removed) = self.remove_message_optimistic(index) {
            self.pending_move_restore
                .insert(intent.envelope_hash, (removed, index));
            return self.dispatch_move(
                intent.envelope_hash,
                intent.source_mailbox,
                intent.dest_mailbox,
            );
        }
        self.try_run_next_move_intent()
    }

    fn trash_intent_for_index(&mut self, index: usize) -> Option<PendingMoveIntent> {
        if let Some(msg) = self.messages.get(index) {
            let mailbox_hash = msg.mailbox_hash;
            if let Some(folder_map) = self.folder_map_for_mailbox(mailbox_hash) {
                if let Some(trash_hash) = folder_map
                    .get("Trash")
                    .or_else(|| folder_map.get("INBOX.Trash"))
                    .copied()
                {
                    return Some(PendingMoveIntent {
                        envelope_hash: msg.envelope_hash,
                        source_mailbox: msg.mailbox_hash,
                        dest_mailbox: trash_hash,
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
            if let Some(folder_map) = self.folder_map_for_mailbox(mailbox_hash) {
                if let Some(archive_hash) = folder_map
                    .get("Archive")
                    .or_else(|| folder_map.get("INBOX.Archive"))
                    .copied()
                {
                    return Some(PendingMoveIntent {
                        envelope_hash: msg.envelope_hash,
                        source_mailbox: msg.mailbox_hash,
                        dest_mailbox: archive_hash,
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
    fn dispatch_move(&mut self, envelope_hash: u64, source_mailbox: u64, dest_mailbox: u64) -> Task<Message> {
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
            self.mutation_in_flight = true;
            self.mutation_epoch = self.mutation_epoch.saturating_add(1);
            let epoch = self.mutation_epoch;
            self.pending_move_epochs.insert(envelope_hash, epoch);
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
                    source_mailbox,
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
    use crate::app::{FlagIntentKind, PendingFlagIntent, PendingMoveIntent};

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
            envelope_hash: 1,
            mailbox_hash: 10,
            kind: FlagIntentKind::ToggleRead,
        };
        let second = PendingFlagIntent {
            envelope_hash: 2,
            mailbox_hash: 10,
            kind: FlagIntentKind::ToggleStar,
        };

        assert_eq!(queue_latest_intent(false, &mut pending, first), Some(first));
        assert_eq!(pending, None);
        assert_eq!(queue_latest_intent(true, &mut pending, first), None);
        assert_eq!(pending, Some(first));
        assert_eq!(queue_latest_intent(true, &mut pending, second), None);
        assert_eq!(pending, Some(second));
    }

    #[test]
    fn mutation_lane_queue_keeps_only_latest_pending_intent() {
        let mut pending: Option<PendingMoveIntent> = None;
        let first = PendingMoveIntent {
            envelope_hash: 7,
            source_mailbox: 11,
            dest_mailbox: 22,
        };
        let second = PendingMoveIntent {
            envelope_hash: 8,
            source_mailbox: 11,
            dest_mailbox: 33,
        };

        assert_eq!(queue_latest_intent(false, &mut pending, first), Some(first));
        assert_eq!(pending, None);
        assert_eq!(queue_latest_intent(true, &mut pending, first), None);
        assert_eq!(pending, Some(first));
        assert_eq!(queue_latest_intent(true, &mut pending, second), None);
        assert_eq!(pending, Some(second));
    }
}
