use cosmic::app::Task;
use futures::future::{AbortHandle, Abortable};
use neverlight_mail_core::store::DEFAULT_PAGE_SIZE;
use std::time::{Duration, Instant};

use super::{AppModel, ConnectionState, Message, Phase};

struct CachedMessagesContext<'a> {
    epoch: u64,
    account_id: Option<&'a str>,
    mailbox_id: Option<&'a str>,
    offset: u32,
}

fn should_apply_cached_messages(
    current: &CachedMessagesContext<'_>,
    incoming: &CachedMessagesContext<'_>,
) -> bool {
    current.epoch == incoming.epoch
        && current.account_id == incoming.account_id
        && current.mailbox_id == incoming.mailbox_id
        && current.offset == incoming.offset
}

fn should_queue_refresh(refresh_in_flight: bool) -> bool {
    refresh_in_flight
}

fn mark_refresh_account_complete(
    outstanding: &mut std::collections::HashSet<String>,
    account_id: &str,
) -> bool {
    outstanding.remove(account_id);
    outstanding.is_empty()
}

const REFRESH_STUCK_TIMEOUT: Duration = Duration::from_secs(45);

fn refresh_has_timed_out(
    refresh_started_at: Option<Instant>,
    refresh_timeout_reported: bool,
) -> bool {
    if refresh_timeout_reported {
        return false;
    }
    refresh_started_at.is_some_and(|started| started.elapsed() >= REFRESH_STUCK_TIMEOUT)
}

impl AppModel {
    pub(super) fn handle_sync(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::CachedFoldersLoaded { account_id, result: Ok(folders) } => {
                if !folders.is_empty() {
                    if let Some(idx) = self.account_index(&account_id) {
                        self.accounts[idx].folders = folders;
                        self.accounts[idx].rebuild_folder_map();

                        // Auto-select INBOX of first account if nothing selected
                        if self.active_account.is_none() {
                            if let Some(fi) = self.accounts[idx].folders.iter().position(|f| f.path == "INBOX") {
                                self.active_account = Some(idx);
                                self.selected_folder = Some(fi);
                                self.selected_mailbox_id =
                                    Some(self.accounts[idx].folders[fi].mailbox_id.clone());
                                self.selected_folder_evicted = false;
                                let mailbox_id = self.accounts[idx].folders[fi].mailbox_id.clone();
                                if let Some(cache) = &self.cache {
                                    let cache = cache.clone();
                                    let aid = account_id.clone();
                                    self.folder_epoch = self.folder_epoch.saturating_add(1);
                                    let epoch = self.folder_epoch;
                                    self.messages_offset = 0;
                                    if let Some(handle) = self.folder_abort.take() {
                                        handle.abort();
                                    }
                                    let (abort_handle, abort_reg) = AbortHandle::new_pair();
                                    self.folder_abort = Some(abort_handle);
                                    return cosmic::task::future(async move {
                                        match Abortable::new(
                                            cache.load_messages(
                                                aid.clone(),
                                                mailbox_id.clone(),
                                                DEFAULT_PAGE_SIZE,
                                                0,
                                            ),
                                            abort_reg,
                                        )
                                        .await
                                        {
                                            Ok(result) => Message::CachedMessagesLoaded {
                                                account_id: aid,
                                                mailbox_id,
                                                offset: 0,
                                                epoch,
                                                result,
                                            },
                                            Err(_) => Message::Noop,
                                        }
                                    });
                                }
                            }
                        }

                        self.status_message = format!(
                            "{} folders (cached)",
                            self.accounts[idx].folders.len()
                        );
                        self.revalidate_selected_folder();
                    }
                }
            }
            Message::CachedFoldersLoaded { result: Err(e), .. } => {
                log::warn!("Failed to load cached folders: {}", e);
            }

            Message::CachedMessagesLoaded {
                account_id,
                ref mailbox_id,
                offset,
                epoch,
                result: Ok(messages),
            } => {
                let active_account_id = self
                    .active_account
                    .and_then(|i| self.accounts.get(i))
                    .map(|a| a.config.id.as_str());
                let active_mailbox_id = self.selected_folder.and_then(|fi| {
                    self.active_account
                        .and_then(|ai| self.accounts.get(ai))
                        .and_then(|a| a.folders.get(fi))
                        .map(|f| f.mailbox_id.as_str())
                });
                let current = CachedMessagesContext {
                    epoch: self.folder_epoch,
                    account_id: active_account_id,
                    mailbox_id: active_mailbox_id,
                    offset: self.messages_offset,
                };
                let incoming = CachedMessagesContext {
                    epoch,
                    account_id: Some(account_id.as_str()),
                    mailbox_id: Some(mailbox_id.as_str()),
                    offset,
                };
                if !should_apply_cached_messages(&current, &incoming) {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }

                let count = messages.len();
                self.has_more_messages = count as u32 == DEFAULT_PAGE_SIZE;
                self.folder_abort = None;

                // Remember selected message by email_id so we can restore
                // selection after the list is replaced (e.g. on refresh).
                let prev_email_id = self.selected_message.and_then(|i| {
                    self.messages.get(i).map(|m| m.email_id.clone())
                });

                if self.messages_offset == 0 {
                    self.messages = messages;
                } else {
                    self.messages.extend(messages);
                }

                // Restore selection by email_id
                if self.messages_offset == 0 {
                    if let Some(ref eid) = prev_email_id {
                        self.selected_message = self
                            .messages
                            .iter()
                            .position(|m| m.email_id == *eid);
                    }
                }

                self.recompute_visible();

                // Reconcile sidebar unread count from actual message flags
                if self.messages_offset == 0 {
                    self.reconcile_folder_unread_count(&account_id, mailbox_id);
                }

                if !self.messages.is_empty() {
                    self.status_message =
                        format!("{} messages", self.messages.len());
                }
                self.phase = Phase::Idle;
            }
            Message::CachedMessagesLoaded { epoch, result: Err(e), .. } => {
                if epoch != self.folder_epoch {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.folder_abort = None;
                log::warn!("Failed to load cached messages: {}", e);
            }

            Message::AccountConnected { account_id, result: Ok(client) } => {
                if let Some(idx) = self.account_index(&account_id) {
                    self.accounts[idx].client = Some(client.clone());
                    self.accounts[idx].conn_state = ConnectionState::Syncing;
                    if self.accounts[idx].reconnect_attempts > 0 {
                        self.reconnect_count = self.reconnect_count.saturating_add(1);
                    }
                    self.accounts[idx].reconnect_attempts = 0;
                    self.accounts[idx].last_error = None;
                    self.notified_messages.clear();
                    self.clear_error_surface();

                    let had_cached_folders = !self.accounts[idx].folders.is_empty();

                    if !had_cached_folders {
                        self.status_message = format!("{}: Connected. Loading folders...", self.accounts[idx].config.label);
                    } else {
                        self.status_message = format!(
                            "{}: {} folders (syncing...)",
                            self.accounts[idx].config.label,
                            self.accounts[idx].folders.len()
                        );
                    }
                    self.phase = Phase::Loading;

                    let cache = self.cache.clone();
                    let aid = account_id.clone();
                    let mut tasks: Vec<Task<Message>> = Vec::new();

                    self.refresh_epoch = self.refresh_epoch.saturating_add(1);
                    let epoch = self.refresh_epoch;
                    tasks.push(cosmic::task::future(async move {
                        let result = neverlight_mail_core::mailbox::fetch_all(&client)
                            .await
                            .map_err(|e| e.to_string());
                        if let (Some(cache), Ok(ref folders)) = (&cache, &result) {
                            if let Err(e) = cache.save_folders(aid.clone(), folders.clone()).await {
                                log::warn!("Failed to cache folders: {}", e);
                            }
                        }
                        Message::SyncFoldersComplete {
                            account_id: aid,
                            epoch,
                            result,
                        }
                    }));

                    // Flush any body view that was deferred while disconnected
                    if let Some(index) = self.pending_body.take() {
                        tasks.push(self.dispatch(Message::ViewBody(index)));
                    }

                    return cosmic::task::batch(tasks);
                }
            }
            Message::AccountConnected { account_id, result: Err(e) } => {
                if let Some(idx) = self.account_index(&account_id) {
                    self.accounts[idx].conn_state = ConnectionState::Error(e.clone());
                    self.accounts[idx].last_error = Some(e.clone());
                    self.accounts[idx].reconnect_attempts = self.accounts[idx].reconnect_attempts.saturating_add(1);
                    log::error!(
                        "JMAP connection failed for '{}' (attempt {}): {}",
                        self.accounts[idx].config.label,
                        self.accounts[idx].reconnect_attempts,
                        e,
                    );

                    let has_folders = !self.accounts[idx].folders.is_empty();
                    let label = &self.accounts[idx].config.label;

                    if !has_folders && self.setup_model.is_none() && self.accounts.len() == 1 {
                        // Single account, no cached data — re-show with error
                        let mut model = neverlight_mail_core::setup::SetupModel::from_config_needs(
                            &neverlight_mail_core::config::ConfigNeedsInput::FullSetup,
                        );
                        model.error = Some(format!("Connection failed: {e}"));
                        self.setup_model = Some(model);
                    }

                    if !has_folders {
                        self.set_status_error(format!("{}: Connection failed: {}", label, e));
                    } else {
                        self.status_message = format!(
                            "{}: {} folders (offline — {})",
                            label,
                            self.accounts[idx].folders.len(),
                            e
                        );
                    }

                    // Schedule reconnect with exponential backoff
                    let delay = self.accounts[idx].reconnect_backoff();
                    let aid = account_id.clone();
                    log::info!(
                        "Scheduling reconnect for '{}' in {}s",
                        self.accounts[idx].config.label,
                        delay.as_secs(),
                    );
                    return cosmic::task::future(async move {
                        tokio::time::sleep(delay).await;
                        Message::ForceReconnect(aid)
                    });
                }
            }

            Message::SyncFoldersComplete {
                account_id,
                epoch,
                result: Ok(folders),
            } => {
                if epoch != self.refresh_epoch {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                let mut refresh_completed = false;
                if self.refresh_in_flight
                    && mark_refresh_account_complete(
                        &mut self.refresh_accounts_outstanding,
                        account_id.as_str(),
                    )
                {
                    self.refresh_in_flight = false;
                    self.refresh_started_at = None;
                    self.refresh_timeout_reported = false;
                    self.phase = Phase::Idle;
                    refresh_completed = true;
                }
                if let Some(idx) = self.account_index(&account_id) {
                    self.accounts[idx].folders = folders;
                    self.accounts[idx].rebuild_folder_map();
                    self.accounts[idx].conn_state = ConnectionState::Connected;
                    self.clear_error_surface();
                    self.last_refresh_at = Some(Instant::now());
                    self.status_message = format!(
                        "{}: {} folders",
                        self.accounts[idx].config.label,
                        self.accounts[idx].folders.len()
                    );

                    // Auto-select INBOX if this is the active account and no folder selected
                    if self.active_account == Some(idx) && self.selected_folder.is_none() {
                        if let Some(fi) = self.accounts[idx].folders.iter().position(|f| f.path == "INBOX") {
                            self.selected_folder = Some(fi);
                            self.selected_mailbox_id =
                                Some(self.accounts[idx].folders[fi].mailbox_id.clone());
                            self.selected_folder_evicted = false;
                        }
                    }
                    // If no active account yet, select this one
                    if self.active_account.is_none() {
                        self.active_account = Some(idx);
                        if let Some(fi) = self.accounts[idx].folders.iter().position(|f| f.path == "INBOX") {
                            self.selected_folder = Some(fi);
                            self.selected_mailbox_id =
                                Some(self.accounts[idx].folders[fi].mailbox_id.clone());
                            self.selected_folder_evicted = false;
                        }
                    }
                    self.revalidate_selected_folder();

                    // If this is the active account, sync the selected folder's messages
                    if self.active_account == Some(idx) {
                        if let Some(fi) = self.selected_folder {
                            if let Some(folder) = self.accounts[idx].folders.get(fi) {
                                let mailbox_id = folder.mailbox_id.clone();
                                if let Some(client) = &self.accounts[idx].client {
                                    let client = client.clone();
                                    let cache = self.cache.clone();
                                    let mid = mailbox_id.clone();
                                    let aid = account_id.clone();
                                    let aid_for_cache = aid.clone();
                                    self.message_epoch = self.message_epoch.saturating_add(1);
                                    let message_epoch = self.message_epoch;
                                    if let Some(handle) = self.message_abort.take() {
                                        handle.abort();
                                    }
                                    let (abort_handle, abort_reg) = AbortHandle::new_pair();
                                    self.message_abort = Some(abort_handle);
                                    let fetch_task = cosmic::task::future(async move {
                                        let result = match Abortable::new(
                                            neverlight_mail_core::email::query_and_get(
                                                &client,
                                                &mid,
                                                DEFAULT_PAGE_SIZE,
                                                0,
                                            ),
                                            abort_reg,
                                        )
                                        .await
                                        {
                                            Ok(result) => result.map(|(msgs, _query_result)| msgs).map_err(|e| e.to_string()),
                                            Err(_) => return Message::Noop,
                                        };
                                        if let (Some(cache), Ok(ref msgs)) = (&cache, &result) {
                                            if let Err(e) =
                                                cache.save_messages(aid_for_cache, mid.clone(), msgs.clone()).await
                                            {
                                                log::warn!("Failed to cache messages: {}", e);
                                            }
                                        }
                                        match result {
                                            Ok(_) => Message::SyncMessagesComplete {
                                                account_id: aid,
                                                mailbox_id: mid,
                                                epoch: message_epoch,
                                                result: Ok(()),
                                            },
                                            Err(e) => Message::SyncMessagesComplete {
                                                account_id: aid,
                                                mailbox_id: mid,
                                                epoch: message_epoch,
                                                result: Err(e),
                                            },
                                        }
                                    });
                                    if refresh_completed && self.refresh_pending {
                                        self.refresh_pending = false;
                                        let refresh_task = self.dispatch(Message::Refresh);
                                        return cosmic::task::batch(vec![fetch_task, refresh_task]);
                                    }
                                    return fetch_task;
                                }
                            }
                        }
                    }
                    if refresh_completed && self.refresh_pending {
                        self.refresh_pending = false;
                        return self.dispatch(Message::Refresh);
                    }
                }
                if refresh_completed && self.refresh_pending {
                    self.refresh_pending = false;
                    return self.dispatch(Message::Refresh);
                }
            }
            Message::SyncFoldersComplete {
                account_id,
                epoch,
                result: Err(e),
            } => {
                if epoch != self.refresh_epoch {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                let mut tasks: Vec<Task<Message>> = Vec::new();
                if let Some(idx) = self.account_index(&account_id) {
                    // Sync failure likely means the connection is dead — invalidate it
                    self.accounts[idx].conn_state = ConnectionState::Error(e.clone());
                    self.accounts[idx].last_error = Some(e.clone());
                    self.accounts[idx].client = None;
                    let label = &self.accounts[idx].config.label;
                    if self.accounts[idx].folders.is_empty() {
                        self.status_message = format!("{}: Failed to load folders: {}", label, e);
                    } else {
                        self.status_message = format!(
                            "{}: {} folders (sync failed: {})",
                            label,
                            self.accounts[idx].folders.len(),
                            e
                        );
                    }
                    log::error!("Folder sync failed for '{}': {} — dropping client", label, e);
                    self.set_status_error(self.status_message.clone());

                    // Schedule reconnect with backoff
                    let delay = self.accounts[idx].reconnect_backoff();
                    let aid = account_id.clone();
                    tasks.push(cosmic::task::future(async move {
                        tokio::time::sleep(delay).await;
                        Message::ForceReconnect(aid)
                    }));

                    if self.refresh_in_flight
                        && mark_refresh_account_complete(
                            &mut self.refresh_accounts_outstanding,
                            account_id.as_str(),
                        )
                    {
                        self.refresh_in_flight = false;
                        self.refresh_started_at = None;
                        self.refresh_timeout_reported = false;
                        if self.refresh_pending {
                            self.refresh_pending = false;
                            tasks.push(self.dispatch(Message::Refresh));
                        }
                    }
                }
                if !tasks.is_empty() {
                    return cosmic::task::batch(tasks);
                }
            }

            Message::SyncMessagesComplete {
                account_id,
                ref mailbox_id,
                epoch,
                result: Ok(()),
            } => {
                if epoch != self.message_epoch {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                if self
                    .active_account
                    .and_then(|i| self.accounts.get(i))
                    .map(|a| a.config.id.as_str())
                    != Some(account_id.as_str())
                    || self.selected_folder
                        .and_then(|fi| {
                            self.active_account
                                .and_then(|ai| self.accounts.get(ai))
                                .and_then(|a| a.folders.get(fi))
                                .map(|f| f.mailbox_id.as_str())
                        }) != Some(mailbox_id.as_str())
                {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                if let Some(idx) = self.active_account {
                    if let Some(acct) = self.accounts.get_mut(idx) {
                        acct.conn_state = ConnectionState::Connected;
                    }
                }
                self.clear_error_surface();
                self.phase = Phase::Idle;
                self.message_abort = None;
                self.last_sync_at = Some(Instant::now());
                let mut tasks: Vec<Task<Message>> = Vec::new();

                if let Some(acct_idx) = self.active_account {
                    if let Some(fi) = self.selected_folder {
                        if let Some(folder) = self.accounts.get(acct_idx).and_then(|a| a.folders.get(fi)) {
                            let mailbox_id = folder.mailbox_id.clone();
                            if let Some(cache) = &self.cache {
                                let cache = cache.clone();
                                let aid = self.active_account_id();
                                self.messages_offset = 0;
                                self.folder_epoch = self.folder_epoch.saturating_add(1);
                                let folder_epoch = self.folder_epoch;
                                if let Some(handle) = self.folder_abort.take() {
                                    handle.abort();
                                }
                                let (abort_handle, abort_reg) = AbortHandle::new_pair();
                                self.folder_abort = Some(abort_handle);
                                tasks.push(cosmic::task::future(async move {
                                    match Abortable::new(
                                        cache.load_messages(
                                            aid.clone(),
                                            mailbox_id.clone(),
                                            DEFAULT_PAGE_SIZE,
                                            0,
                                        ),
                                        abort_reg,
                                    )
                                    .await
                                    {
                                        Ok(result) => Message::CachedMessagesLoaded {
                                            account_id: aid,
                                            mailbox_id,
                                            offset: 0,
                                            epoch: folder_epoch,
                                            result,
                                        },
                                        Err(_) => Message::Noop,
                                    }
                                }));
                            }
                        }
                    }
                }

                // Flush any body view deferred while sync was in progress
                if let Some(index) = self.pending_body.take() {
                    tasks.push(self.dispatch(Message::ViewBody(index)));
                }

                if tasks.is_empty() {
                    self.status_message =
                        format!("{} messages (synced)", self.messages.len());
                }

                if !tasks.is_empty() {
                    return cosmic::task::batch(tasks);
                }
            }
            Message::SyncMessagesComplete { ref account_id, epoch, result: Err(ref e), .. } => {
                if epoch != self.message_epoch {
                    self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
                    return Task::none();
                }
                self.message_abort = None;
                if let Some(idx) = self.account_index(account_id) {
                    let acct = &mut self.accounts[idx];
                    acct.conn_state = ConnectionState::Error(e.clone());
                    acct.last_error = Some(e.clone());
                    acct.client = None;
                    let label = &acct.config.label;
                    log::error!("Message sync failed for '{}': {} — dropping client", label, e);

                    let delay = acct.reconnect_backoff();
                    let aid = account_id.clone();
                    self.status_message = format!("Sync failed: {}", e);
                    self.set_status_error(self.status_message.clone());
                    return cosmic::task::future(async move {
                        tokio::time::sleep(delay).await;
                        Message::ForceReconnect(aid)
                    });
                }
                self.status_message = format!("Sync failed: {}", e);
                log::error!("Message sync failed: {}", e);
                self.set_status_error(self.status_message.clone());
            }

            Message::SelectFolder(acct_idx, folder_idx) => {
                self.active_account = Some(acct_idx);
                self.selected_folder = Some(folder_idx);
                self.selected_mailbox_id = self
                    .accounts
                    .get(acct_idx)
                    .and_then(|acct| acct.folders.get(folder_idx))
                    .map(|f| f.mailbox_id.clone());
                self.selected_folder_evicted = false;
                if let Some(handle) = self.folder_abort.take() {
                    handle.abort();
                }
                if let Some(handle) = self.message_abort.take() {
                    handle.abort();
                }
                self.folder_epoch = self.folder_epoch.saturating_add(1);
                let folder_epoch = self.folder_epoch;
                self.message_epoch = self.message_epoch.saturating_add(1);
                let message_epoch = self.message_epoch;
                self.messages.clear();
                self.selected_message = None;
                self.preview_body.clear();
                self.preview_markdown.clear();
                self.preview_attachments.clear();
                self.preview_image_handles.clear();
                self.messages_offset = 0;
                self.has_more_messages = false;
                self.collapsed_threads.clear();
                self.recompute_visible();
                self.phase = Phase::Loading;

                if let Some(acct) = self.accounts.get(acct_idx) {
                    if let Some(folder) = acct.folders.get(folder_idx) {
                        let mailbox_id = folder.mailbox_id.clone();
                        let folder_name = folder.name.clone();
                        let aid = acct.config.id.clone();
                        let mut tasks: Vec<Task<Message>> = Vec::new();

                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let aid2 = aid.clone();
                            let mid = mailbox_id.clone();
                            let (abort_handle, abort_reg) = AbortHandle::new_pair();
                            self.folder_abort = Some(abort_handle);
                            tasks.push(cosmic::task::future(async move {
                                match Abortable::new(
                                    cache.load_messages(
                                        aid2.clone(),
                                        mid.clone(),
                                        DEFAULT_PAGE_SIZE,
                                        0,
                                    ),
                                    abort_reg,
                                )
                                .await
                                {
                                    Ok(result) => Message::CachedMessagesLoaded {
                                        account_id: aid2,
                                        mailbox_id: mid,
                                        offset: 0,
                                        epoch: folder_epoch,
                                        result,
                                    },
                                    Err(_) => Message::Noop,
                                }
                            }));
                        }

                        if let Some(client) = &acct.client {
                            let client = client.clone();
                            let cache = self.cache.clone();
                            let aid2 = aid.clone();
                            let aid_for_cache = aid2.clone();
                            let mid = mailbox_id.clone();
                            let mid_for_cache = mid.clone();
                            if let Some(acct_mut) = self.accounts.get_mut(acct_idx) {
                                acct_mut.conn_state = ConnectionState::Syncing;
                            }
                            self.status_message = format!("Loading {}...", folder_name);
                            let (abort_handle, abort_reg) = AbortHandle::new_pair();
                            self.message_abort = Some(abort_handle);
                            tasks.push(cosmic::task::future(async move {
                                let result = match Abortable::new(
                                    neverlight_mail_core::email::query_and_get(
                                        &client,
                                        &mid,
                                        DEFAULT_PAGE_SIZE,
                                        0,
                                    ),
                                    abort_reg,
                                )
                                .await
                                {
                                    Ok(result) => result.map(|(msgs, _query_result)| msgs).map_err(|e| e.to_string()),
                                    Err(_) => return Message::Noop,
                                };
                                if let (Some(cache), Ok(ref msgs)) = (&cache, &result) {
                                    if let Err(e) =
                                        cache.save_messages(aid_for_cache, mid_for_cache, msgs.clone()).await
                                    {
                                        log::warn!("Failed to cache messages: {}", e);
                                    }
                                }
                                match result {
                                    Ok(_) => Message::SyncMessagesComplete {
                                        account_id: aid2,
                                        mailbox_id: mid,
                                        epoch: message_epoch,
                                        result: Ok(()),
                                    },
                                    Err(e) => Message::SyncMessagesComplete {
                                        account_id: aid2,
                                        mailbox_id: mid,
                                        epoch: message_epoch,
                                        result: Err(e),
                                    },
                                }
                            }));
                        }

                        if !tasks.is_empty() {
                            return cosmic::task::batch(tasks);
                        }
                    }
                }
            }

            Message::LoadMoreMessages => {
                self.messages_offset += DEFAULT_PAGE_SIZE;
                let offset = self.messages_offset;

                if let Some(acct_idx) = self.active_account {
                    if let Some(fi) = self.selected_folder {
                        if let Some(folder) = self.accounts.get(acct_idx).and_then(|a| a.folders.get(fi)) {
                            let mailbox_id = folder.mailbox_id.clone();
                            if let Some(cache) = &self.cache {
                                let cache = cache.clone();
                                let aid = self.active_account_id();
                                let mid = mailbox_id.clone();
                                let epoch = self.folder_epoch;
                                if let Some(handle) = self.folder_abort.take() {
                                    handle.abort();
                                }
                                let (abort_handle, abort_reg) = AbortHandle::new_pair();
                                self.folder_abort = Some(abort_handle);
                                return cosmic::task::future(async move {
                                    match Abortable::new(
                                        cache.load_messages(
                                            aid.clone(),
                                            mid.clone(),
                                            DEFAULT_PAGE_SIZE,
                                            offset,
                                        ),
                                        abort_reg,
                                    )
                                    .await
                                    {
                                        Ok(result) => Message::CachedMessagesLoaded {
                                            account_id: aid,
                                            mailbox_id: mid,
                                            offset,
                                            epoch,
                                            result,
                                        },
                                        Err(_) => Message::Noop,
                                    }
                                });
                            }
                        }
                    }
                }
            }

            Message::Refresh => {
                if should_queue_refresh(self.refresh_in_flight) {
                    if refresh_has_timed_out(self.refresh_started_at, self.refresh_timeout_reported) {
                        // Force-clear the stuck refresh so a new one can start
                        self.refresh_timeout_reported = true;
                        self.refresh_timeout_count = self.refresh_timeout_count.saturating_add(1);
                        self.refresh_stuck_count = self.refresh_stuck_count.saturating_add(1);
                        self.refresh_in_flight = false;
                        self.refresh_started_at = None;
                        self.refresh_accounts_outstanding.clear();
                        // Bump epoch so stale completions from the hung refresh are dropped
                        self.refresh_epoch = self.refresh_epoch.saturating_add(1);
                        log::warn!("Refresh stuck ({}s timeout), force-clearing and restarting", REFRESH_STUCK_TIMEOUT.as_secs());
                        // Fall through to start a new refresh below
                    } else {
                        self.refresh_pending = true;
                        self.status_message = "Refresh queued...".into();
                        return Task::none();
                    }
                }
                self.refresh_epoch = self.refresh_epoch.saturating_add(1);
                let refresh_epoch = self.refresh_epoch;
                let mut tasks: Vec<Task<Message>> = Vec::new();
                self.refresh_accounts_outstanding.clear();
                for acct in &self.accounts {
                    if let Some(client) = &acct.client {
                        let client = client.clone();
                        let cache = self.cache.clone();
                        let aid = acct.config.id.clone();
                        self.refresh_accounts_outstanding.insert(aid.clone());
                        tasks.push(cosmic::task::future(async move {
                            let result = neverlight_mail_core::mailbox::fetch_all(&client)
                                .await
                                .map_err(|e| e.to_string());
                            if let (Some(cache), Ok(ref folders)) = (&cache, &result) {
                                if let Err(e) = cache.save_folders(aid.clone(), folders.clone()).await {
                                    log::warn!("Failed to cache folders: {}", e);
                                }
                            }
                            Message::SyncFoldersComplete {
                                account_id: aid,
                                epoch: refresh_epoch,
                                result,
                            }
                        }));
                    }
                }
                if !tasks.is_empty() {
                    self.refresh_in_flight = true;
                    self.refresh_started_at = Some(Instant::now());
                    self.refresh_timeout_reported = false;
                    self.phase = Phase::Refreshing;
                    self.clear_error_surface();
                    self.status_message = "Refreshing...".into();
                    return cosmic::task::batch(tasks);
                }
                self.refresh_started_at = None;
                self.refresh_timeout_reported = false;
                self.phase = Phase::Idle;
            }

            Message::ForceReconnect(ref account_id) => {
                if let Some(idx) = self.account_index(account_id) {
                    let acct = &mut self.accounts[idx];
                    if matches!(acct.conn_state, ConnectionState::Connecting | ConnectionState::Syncing) {
                        return Task::none();
                    }
                    // Ignore stale delayed reconnect tasks once account is already healthy.
                    if acct.client.is_some() && matches!(acct.conn_state, ConnectionState::Connected) {
                        return Task::none();
                    }
                    acct.client = None;
                    acct.conn_state = ConnectionState::Connecting;
                    let config = acct.config.clone();
                    let aid = account_id.clone();
                    self.status_message = format!("{}: Reconnecting...", acct.config.label);
                    return super::connect_account(config, aid);
                }
            }

            _ => {}
        }
        Task::none()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        mark_refresh_account_complete, refresh_has_timed_out, should_apply_cached_messages,
        should_queue_refresh, CachedMessagesContext,
    };
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    fn ctx<'a>(
        epoch: u64,
        account_id: Option<&'a str>,
        mailbox_id: Option<&'a str>,
        offset: u32,
    ) -> CachedMessagesContext<'a> {
        CachedMessagesContext { epoch, account_id, mailbox_id, offset }
    }

    #[test]
    fn cached_messages_apply_when_epoch_and_context_match() {
        let current = ctx(3, Some("acct-1"), Some("mbox-42"), 0);
        let incoming = ctx(3, Some("acct-1"), Some("mbox-42"), 0);
        assert!(should_apply_cached_messages(&current, &incoming));
    }

    #[test]
    fn cached_messages_drop_on_epoch_mismatch() {
        let current = ctx(3, Some("acct-1"), Some("mbox-42"), 0);
        let incoming = ctx(2, Some("acct-1"), Some("mbox-42"), 0);
        assert!(!should_apply_cached_messages(&current, &incoming));
    }

    #[test]
    fn cached_messages_drop_on_account_or_mailbox_or_offset_mismatch() {
        let base = ctx(3, Some("acct-1"), Some("mbox-42"), 0);

        let wrong_account = ctx(3, Some("acct-2"), Some("mbox-42"), 0);
        assert!(!should_apply_cached_messages(&wrong_account, &base));

        let wrong_mailbox = ctx(3, Some("acct-1"), Some("mbox-7"), 0);
        assert!(!should_apply_cached_messages(&wrong_mailbox, &base));

        let wrong_offset = ctx(3, Some("acct-1"), Some("mbox-42"), 50);
        assert!(!should_apply_cached_messages(&wrong_offset, &base));
    }

    #[test]
    fn refresh_is_queued_when_in_flight() {
        assert!(should_queue_refresh(true));
        assert!(!should_queue_refresh(false));
    }

    #[test]
    fn refresh_completion_drains_outstanding_accounts() {
        let mut outstanding: HashSet<String> =
            ["acct-a".to_string(), "acct-b".to_string()].into_iter().collect();
        assert!(!mark_refresh_account_complete(&mut outstanding, "acct-a"));
        assert_eq!(outstanding.len(), 1);
        assert!(mark_refresh_account_complete(&mut outstanding, "acct-b"));
        assert!(outstanding.is_empty());
    }

    #[test]
    fn refresh_timeout_detects_stuck_once_per_cycle() {
        let started = Some(Instant::now() - Duration::from_secs(60));
        assert!(refresh_has_timed_out(started, false));
        assert!(!refresh_has_timed_out(started, true));
        assert!(!refresh_has_timed_out(Some(Instant::now()), false));
    }
}
