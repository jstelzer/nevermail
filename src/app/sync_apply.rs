//! Sync message handlers — extracted from sync.rs handle_sync match arms.

use cosmic::app::Task;
use futures::future::{AbortHandle, Abortable};
use neverlight_mail_core::models::Folder;
use neverlight_mail_core::store::DEFAULT_PAGE_SIZE;
use std::time::Instant;

use super::{AppModel, ConnectionState, Message, Phase};
use super::sync::{mark_refresh_account_complete, refresh_has_timed_out, should_queue_refresh, REFRESH_STUCK_TIMEOUT};

impl AppModel {
    pub(super) fn handle_cached_folders_ok(
        &mut self,
        account_id: String,
        folders: Vec<Folder>,
    ) -> Task<Message> {
        if folders.is_empty() {
            return Task::none();
        }
        let Some(idx) = self.account_index(&account_id) else {
            return Task::none();
        };
        self.accounts[idx].folders = folders;
        self.accounts[idx].rebuild_folder_map();

        // Auto-select INBOX of first account if nothing selected
        if self.active_account.is_some() {
            return Task::none();
        }
        let Some(fi) = self.accounts[idx].folders.iter().position(|f| f.path == "INBOX") else {
            return Task::none();
        };
        self.active_account = Some(idx);
        self.selected_folder = Some(fi);
        self.selected_mailbox_id =
            Some(self.accounts[idx].folders[fi].mailbox_id.clone());
        self.selected_folder_evicted = false;
        let mailbox_id = self.accounts[idx].folders[fi].mailbox_id.clone();
        let Some(cache) = &self.cache else {
            return Task::none();
        };
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
        cosmic::task::future(async move {
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
        })
    }

    pub(super) fn handle_account_connected_ok(
        &mut self,
        account_id: String,
        client: neverlight_mail_core::client::JmapClient,
    ) -> Task<Message> {
        let Some(idx) = self.account_index(&account_id) else {
            return Task::none();
        };
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

        cosmic::task::batch(tasks)
    }

    pub(super) fn handle_account_connected_err(
        &mut self,
        account_id: String,
        e: String,
    ) -> Task<Message> {
        let Some(idx) = self.account_index(&account_id) else {
            return Task::none();
        };
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

        let delay = self.accounts[idx].reconnect_backoff();
        let aid = account_id.clone();
        log::info!(
            "Scheduling reconnect for '{}' in {}s",
            self.accounts[idx].config.label,
            delay.as_secs(),
        );
        cosmic::task::future(async move {
            tokio::time::sleep(delay).await;
            Message::ForceReconnect(aid)
        })
    }

    pub(super) fn handle_sync_folders_ok(
        &mut self,
        account_id: String,
        epoch: u64,
        folders: Vec<Folder>,
    ) -> Task<Message> {
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
        let Some(idx) = self.account_index(&account_id) else {
            if refresh_completed && self.refresh_pending {
                self.refresh_pending = false;
                return self.dispatch(Message::Refresh);
            }
            return Task::none();
        };
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
        let can_fetch = self.active_account == Some(idx)
            && self.selected_folder.is_some()
            && self
                .selected_folder
                .and_then(|fi| self.accounts[idx].folders.get(fi))
                .is_some()
            && self.accounts[idx].client.is_some();

        if can_fetch {
            let fi = self.selected_folder.expect("checked above");
            let mailbox_id = self.accounts[idx].folders[fi].mailbox_id.clone();
            let client = self.accounts[idx].client.clone().expect("checked above");
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

        if refresh_completed && self.refresh_pending {
            self.refresh_pending = false;
            return self.dispatch(Message::Refresh);
        }
        Task::none()
    }

    pub(super) fn handle_sync_folders_err(
        &mut self,
        account_id: String,
        epoch: u64,
        e: String,
    ) -> Task<Message> {
        if epoch != self.refresh_epoch {
            self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
            return Task::none();
        }
        let mut tasks: Vec<Task<Message>> = Vec::new();
        if let Some(idx) = self.account_index(&account_id) {
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
        if tasks.is_empty() {
            Task::none()
        } else {
            cosmic::task::batch(tasks)
        }
    }

    pub(super) fn handle_sync_messages_ok(
        &mut self,
        account_id: String,
        mailbox_id: String,
        epoch: u64,
    ) -> Task<Message> {
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

        let has_cache_reload_target = self.active_account.is_some()
            && self.selected_folder.is_some()
            && self
                .active_account
                .and_then(|ai| self.accounts.get(ai))
                .and_then(|a| self.selected_folder.and_then(|fi| a.folders.get(fi)))
                .is_some()
            && self.cache.is_some();

        if has_cache_reload_target {
            let acct_idx = self.active_account.expect("checked above");
            let fi = self.selected_folder.expect("checked above");
            let mailbox_id = self.accounts[acct_idx].folders[fi].mailbox_id.clone();
            let cache = self.cache.clone().expect("checked above");
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

        if let Some(index) = self.pending_body.take() {
            tasks.push(self.dispatch(Message::ViewBody(index)));
        }

        if tasks.is_empty() {
            self.status_message =
                format!("{} messages (synced)", self.messages.len());
        }

        if tasks.is_empty() {
            Task::none()
        } else {
            cosmic::task::batch(tasks)
        }
    }

    pub(super) fn handle_sync_messages_err(
        &mut self,
        account_id: &str,
        epoch: u64,
        e: &str,
    ) -> Task<Message> {
        if epoch != self.message_epoch {
            self.stale_apply_drop_count = self.stale_apply_drop_count.saturating_add(1);
            return Task::none();
        }
        self.message_abort = None;
        if let Some(idx) = self.account_index(account_id) {
            let acct = &mut self.accounts[idx];
            acct.conn_state = ConnectionState::Error(e.to_string());
            acct.last_error = Some(e.to_string());
            acct.client = None;
            let label = &acct.config.label;
            log::error!("Message sync failed for '{}': {} — dropping client", label, e);

            let delay = acct.reconnect_backoff();
            let aid = account_id.to_string();
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
        Task::none()
    }

    pub(super) fn handle_select_folder(
        &mut self,
        acct_idx: usize,
        folder_idx: usize,
    ) -> Task<Message> {
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

        let Some(acct) = self.accounts.get(acct_idx) else {
            return Task::none();
        };
        let Some(folder) = acct.folders.get(folder_idx) else {
            return Task::none();
        };
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

        if tasks.is_empty() {
            Task::none()
        } else {
            cosmic::task::batch(tasks)
        }
    }

    pub(super) fn handle_load_more_messages(&mut self) -> Task<Message> {
        self.messages_offset += DEFAULT_PAGE_SIZE;
        let offset = self.messages_offset;

        let Some(acct_idx) = self.active_account else {
            return Task::none();
        };
        let Some(fi) = self.selected_folder else {
            return Task::none();
        };
        let Some(folder) = self.accounts.get(acct_idx).and_then(|a| a.folders.get(fi)) else {
            return Task::none();
        };
        let mailbox_id = folder.mailbox_id.clone();
        let Some(cache) = &self.cache else {
            return Task::none();
        };
        let cache = cache.clone();
        let aid = self.active_account_id();
        let mid = mailbox_id.clone();
        let epoch = self.folder_epoch;
        if let Some(handle) = self.folder_abort.take() {
            handle.abort();
        }
        let (abort_handle, abort_reg) = AbortHandle::new_pair();
        self.folder_abort = Some(abort_handle);
        cosmic::task::future(async move {
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
        })
    }

    pub(super) fn handle_refresh(&mut self) -> Task<Message> {
        if should_queue_refresh(self.refresh_in_flight) {
            if refresh_has_timed_out(self.refresh_started_at, self.refresh_timeout_reported) {
                self.refresh_timeout_reported = true;
                self.refresh_timeout_count = self.refresh_timeout_count.saturating_add(1);
                self.refresh_stuck_count = self.refresh_stuck_count.saturating_add(1);
                self.refresh_in_flight = false;
                self.refresh_started_at = None;
                self.refresh_accounts_outstanding.clear();
                self.refresh_epoch = self.refresh_epoch.saturating_add(1);
                log::warn!("Refresh stuck ({}s timeout), force-clearing and restarting", REFRESH_STUCK_TIMEOUT.as_secs());
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
        Task::none()
    }

    pub(super) fn handle_force_reconnect(&mut self, account_id: &str) -> Task<Message> {
        let Some(idx) = self.account_index(account_id) else {
            return Task::none();
        };
        let acct = &mut self.accounts[idx];
        if matches!(acct.conn_state, ConnectionState::Connecting | ConnectionState::Syncing) {
            return Task::none();
        }
        if acct.client.is_some() && matches!(acct.conn_state, ConnectionState::Connected) {
            return Task::none();
        }
        acct.client = None;
        acct.conn_state = ConnectionState::Connecting;
        let config = acct.config.clone();
        let aid = account_id.to_string();
        self.status_message = format!("{}: Reconnecting...", acct.config.label);
        super::connect_account(config, aid)
    }
}
