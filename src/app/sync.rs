use cosmic::app::Task;
use neverlight_mail_core::MailboxHash;
use neverlight_mail_core::imap::ImapSession;
use neverlight_mail_core::store::DEFAULT_PAGE_SIZE;

use super::{AppModel, ConnectionState, Message};

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
                                let mailbox_hash = self.accounts[idx].folders[fi].mailbox_hash;
                                if let Some(cache) = &self.cache {
                                    let cache = cache.clone();
                                    let aid = account_id.clone();
                                    self.messages_offset = 0;
                                    return cosmic::task::future(async move {
                                        Message::CachedMessagesLoaded(
                                            cache
                                                .load_messages(aid, mailbox_hash, DEFAULT_PAGE_SIZE, 0)
                                                .await,
                                        )
                                    });
                                }
                            }
                        }

                        self.status_message = format!(
                            "{} folders (cached)",
                            self.accounts[idx].folders.len()
                        );
                    }
                }
            }
            Message::CachedFoldersLoaded { result: Err(e), .. } => {
                log::warn!("Failed to load cached folders: {}", e);
            }

            Message::CachedMessagesLoaded(Ok(messages)) => {
                let count = messages.len();
                self.has_more_messages = count as u32 == DEFAULT_PAGE_SIZE;

                if self.messages_offset == 0 {
                    self.messages = messages;
                } else {
                    self.messages.extend(messages);
                }

                self.recompute_visible();

                if !self.messages.is_empty() {
                    self.status_message =
                        format!("{} messages", self.messages.len());
                }
            }
            Message::CachedMessagesLoaded(Err(e)) => {
                log::warn!("Failed to load cached messages: {}", e);
            }

            Message::AccountConnected { account_id, result: Ok(session) } => {
                if let Some(idx) = self.account_index(&account_id) {
                    self.accounts[idx].session = Some(session.clone());
                    self.accounts[idx].conn_state = ConnectionState::Syncing;

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

                    let cache = self.cache.clone();
                    let aid = account_id.clone();
                    let mut tasks: Vec<Task<Message>> = Vec::new();

                    tasks.push(cosmic::task::future(async move {
                        let result = session.fetch_folders().await;
                        if let (Some(cache), Ok(ref folders)) = (&cache, &result) {
                            if let Err(e) = cache.save_folders(aid.clone(), folders.clone()).await {
                                log::warn!("Failed to cache folders: {}", e);
                            }
                        }
                        Message::SyncFoldersComplete { account_id: aid, result }
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
                    log::error!("IMAP connection failed for '{}': {}", self.accounts[idx].config.label, e);

                    let has_folders = !self.accounts[idx].folders.is_empty();
                    let label = &self.accounts[idx].config.label;

                    if !has_folders && !self.show_setup_dialog && self.accounts.len() == 1 {
                        // Single account, no cached data — re-show with error
                        self.show_setup_dialog = true;
                        self.password_only_mode = false;
                        self.setup_error = Some(format!("Connection failed: {e}"));
                    }

                    if !has_folders {
                        self.status_message = format!("{}: Connection failed: {}", label, e);
                    } else {
                        self.status_message = format!(
                            "{}: {} folders (offline — {})",
                            label,
                            self.accounts[idx].folders.len(),
                            e
                        );
                    }
                }
            }

            Message::SyncFoldersComplete { account_id, result: Ok(folders) } => {
                if let Some(idx) = self.account_index(&account_id) {
                    self.accounts[idx].folders = folders;
                    self.accounts[idx].rebuild_folder_map();
                    self.accounts[idx].conn_state = ConnectionState::Connected;
                    self.status_message = format!(
                        "{}: {} folders",
                        self.accounts[idx].config.label,
                        self.accounts[idx].folders.len()
                    );

                    // Auto-select INBOX if this is the active account and no folder selected
                    if self.active_account == Some(idx) && self.selected_folder.is_none() {
                        if let Some(fi) = self.accounts[idx].folders.iter().position(|f| f.path == "INBOX") {
                            self.selected_folder = Some(fi);
                        }
                    }
                    // If no active account yet, select this one
                    if self.active_account.is_none() {
                        self.active_account = Some(idx);
                        if let Some(fi) = self.accounts[idx].folders.iter().position(|f| f.path == "INBOX") {
                            self.selected_folder = Some(fi);
                        }
                    }

                    // If this is the active account, sync the selected folder's messages
                    if self.active_account == Some(idx) {
                        if let Some(fi) = self.selected_folder {
                            if let Some(folder) = self.accounts[idx].folders.get(fi) {
                                let mailbox_hash = MailboxHash(folder.mailbox_hash);
                                if let Some(session) = &self.accounts[idx].session {
                                    let session = session.clone();
                                    let cache = self.cache.clone();
                                    let mh = folder.mailbox_hash;
                                    let aid = account_id.clone();
                                    return cosmic::task::future(async move {
                                        let result = session.fetch_messages(mailbox_hash).await;
                                        if let (Some(cache), Ok(ref msgs)) = (&cache, &result) {
                                            if let Err(e) =
                                                cache.save_messages(aid, mh, msgs.clone()).await
                                            {
                                                log::warn!("Failed to cache messages: {}", e);
                                            }
                                        }
                                        match result {
                                            Ok(_) => Message::SyncMessagesComplete(Ok(())),
                                            Err(e) => Message::SyncMessagesComplete(Err(e)),
                                        }
                                    });
                                }
                            }
                        }
                    }
                }
            }
            Message::SyncFoldersComplete { account_id, result: Err(e) } => {
                if let Some(idx) = self.account_index(&account_id) {
                    self.accounts[idx].conn_state = ConnectionState::Connected;
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
                    log::error!("Folder sync failed for '{}': {}", label, e);
                }
            }

            Message::SyncMessagesComplete(Ok(())) => {
                if let Some(idx) = self.active_account {
                    if let Some(acct) = self.accounts.get_mut(idx) {
                        acct.conn_state = ConnectionState::Connected;
                    }
                }
                let mut tasks: Vec<Task<Message>> = Vec::new();

                if let Some(acct_idx) = self.active_account {
                    if let Some(fi) = self.selected_folder {
                        if let Some(folder) = self.accounts.get(acct_idx).and_then(|a| a.folders.get(fi)) {
                            let mailbox_hash = folder.mailbox_hash;
                            if let Some(cache) = &self.cache {
                                let cache = cache.clone();
                                let aid = self.active_account_id();
                                self.messages_offset = 0;
                                tasks.push(cosmic::task::future(async move {
                                    Message::CachedMessagesLoaded(
                                        cache
                                            .load_messages(aid, mailbox_hash, DEFAULT_PAGE_SIZE, 0)
                                            .await,
                                    )
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
            Message::SyncMessagesComplete(Err(e)) => {
                if let Some(idx) = self.active_account {
                    if let Some(acct) = self.accounts.get_mut(idx) {
                        acct.conn_state = ConnectionState::Connected;
                    }
                }
                self.status_message = format!("Sync failed: {}", e);
                log::error!("Message sync failed: {}", e);
            }

            Message::SelectFolder(acct_idx, folder_idx) => {
                self.active_account = Some(acct_idx);
                self.selected_folder = Some(folder_idx);
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

                if let Some(acct) = self.accounts.get(acct_idx) {
                    if let Some(folder) = acct.folders.get(folder_idx) {
                        let mailbox_hash = folder.mailbox_hash;
                        let folder_name = folder.name.clone();
                        let aid = acct.config.id.clone();
                        let mut tasks: Vec<Task<Message>> = Vec::new();

                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let aid2 = aid.clone();
                            tasks.push(cosmic::task::future(async move {
                                Message::CachedMessagesLoaded(
                                    cache.load_messages(aid2, mailbox_hash, DEFAULT_PAGE_SIZE, 0).await,
                                )
                            }));
                        }

                        if let Some(session) = &acct.session {
                            let session = session.clone();
                            let cache = self.cache.clone();
                            let aid2 = aid.clone();
                            if let Some(acct_mut) = self.accounts.get_mut(acct_idx) {
                                acct_mut.conn_state = ConnectionState::Syncing;
                            }
                            self.status_message = format!("Loading {}...", folder_name);
                            let mbox_hash = MailboxHash(mailbox_hash);
                            tasks.push(cosmic::task::future(async move {
                                let result = session.fetch_messages(mbox_hash).await;
                                if let (Some(cache), Ok(ref msgs)) = (&cache, &result) {
                                    if let Err(e) =
                                        cache.save_messages(aid2, mailbox_hash, msgs.clone()).await
                                    {
                                        log::warn!("Failed to cache messages: {}", e);
                                    }
                                }
                                match result {
                                    Ok(_) => Message::SyncMessagesComplete(Ok(())),
                                    Err(e) => Message::SyncMessagesComplete(Err(e)),
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
                            let mailbox_hash = folder.mailbox_hash;
                            if let Some(cache) = &self.cache {
                                let cache = cache.clone();
                                let aid = self.active_account_id();
                                return cosmic::task::future(async move {
                                    Message::CachedMessagesLoaded(
                                        cache
                                            .load_messages(aid, mailbox_hash, DEFAULT_PAGE_SIZE, offset)
                                            .await,
                                    )
                                });
                            }
                        }
                    }
                }
            }

            Message::Refresh => {
                let mut tasks: Vec<Task<Message>> = Vec::new();
                for acct in &self.accounts {
                    if let Some(session) = &acct.session {
                        let session = session.clone();
                        let cache = self.cache.clone();
                        let aid = acct.config.id.clone();
                        tasks.push(cosmic::task::future(async move {
                            let result = session.fetch_folders().await;
                            if let (Some(cache), Ok(ref folders)) = (&cache, &result) {
                                if let Err(e) = cache.save_folders(aid.clone(), folders.clone()).await {
                                    log::warn!("Failed to cache folders: {}", e);
                                }
                            }
                            Message::SyncFoldersComplete { account_id: aid, result }
                        }));
                    }
                }
                if !tasks.is_empty() {
                    self.status_message = "Refreshing...".into();
                    return cosmic::task::batch(tasks);
                }
            }

            Message::ForceReconnect(ref account_id) => {
                if let Some(idx) = self.account_index(account_id) {
                    let acct = &mut self.accounts[idx];
                    if matches!(acct.conn_state, ConnectionState::Connecting | ConnectionState::Syncing) {
                        return Task::none();
                    }
                    acct.session = None;
                    acct.conn_state = ConnectionState::Connecting;
                    let config = acct.config.to_imap_config();
                    let aid = account_id.clone();
                    self.status_message = format!("{}: Reconnecting...", acct.config.label);
                    return cosmic::task::future(async move {
                        let result = ImapSession::connect(config).await;
                        Message::AccountConnected { account_id: aid, result }
                    });
                }
            }

            _ => {}
        }
        Task::none()
    }
}
