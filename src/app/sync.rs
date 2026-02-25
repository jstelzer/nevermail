use cosmic::app::Task;
use melib::MailboxHash;

use super::{AppModel, Message};
use crate::core::store::DEFAULT_PAGE_SIZE;

impl AppModel {
    pub(super) fn handle_sync(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::CachedFoldersLoaded(Ok(folders)) => {
                if !folders.is_empty() {
                    self.folders = folders;
                    self.rebuild_folder_map();
                    self.status_message =
                        format!("{} folders (cached)", self.folders.len());

                    // Auto-select INBOX and load cached messages
                    if let Some(idx) = self.folders.iter().position(|f| f.path == "INBOX") {
                        self.selected_folder = Some(idx);
                        let mailbox_hash = self.folders[idx].mailbox_hash;
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            self.messages_offset = 0;
                            return cosmic::task::future(async move {
                                Message::CachedMessagesLoaded(
                                    cache
                                        .load_messages(mailbox_hash, DEFAULT_PAGE_SIZE, 0)
                                        .await,
                                )
                            });
                        }
                    }
                }
            }
            Message::CachedFoldersLoaded(Err(e)) => {
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

            Message::Connected(Ok(session)) => {
                self.session = Some(session.clone());
                let had_cached_folders = !self.folders.is_empty();

                if !had_cached_folders {
                    self.is_syncing = true;
                    self.status_message = "Connected. Loading folders...".into();
                } else {
                    self.status_message = format!(
                        "{} folders (syncing...)",
                        self.folders.len()
                    );
                }

                let cache = self.cache.clone();
                return cosmic::task::future(async move {
                    let result = session.fetch_folders().await;
                    if let (Some(cache), Ok(ref folders)) = (&cache, &result) {
                        if let Err(e) = cache.save_folders(folders.clone()).await {
                            log::warn!("Failed to cache folders: {}", e);
                        }
                    }
                    Message::SyncFoldersComplete(result)
                });
            }
            Message::Connected(Err(e)) => {
                self.is_syncing = false;
                log::error!("IMAP connection failed: {}", e);

                if self.folders.is_empty() && !self.show_setup_dialog {
                    // No cached data and not already showing dialog — re-show with error
                    self.show_setup_dialog = true;
                    // Preserve password_only_mode from previous state if config exists,
                    // otherwise show full setup
                    if self.config.is_some() {
                        self.password_only_mode = false;
                    }
                    self.setup_error = Some(format!("Connection failed: {e}"));
                    self.status_message = format!("Connection failed: {}", e);
                } else if self.folders.is_empty() {
                    self.status_message = format!("Connection failed: {}", e);
                } else {
                    self.status_message = format!(
                        "{} folders (offline — {})",
                        self.folders.len(),
                        e
                    );
                }
            }

            Message::SyncFoldersComplete(Ok(folders)) => {
                self.folders = folders;
                self.rebuild_folder_map();
                self.is_syncing = false;
                self.status_message = format!("{} folders", self.folders.len());

                if self.selected_folder.is_none() {
                    if let Some(idx) = self.folders.iter().position(|f| f.path == "INBOX") {
                        self.selected_folder = Some(idx);
                    }
                }

                if let Some(idx) = self.selected_folder {
                    if let Some(folder) = self.folders.get(idx) {
                        let mailbox_hash = MailboxHash(folder.mailbox_hash);
                        if let Some(session) = &self.session {
                            let session = session.clone();
                            let cache = self.cache.clone();
                            let mh = folder.mailbox_hash;
                            return cosmic::task::future(async move {
                                let result = session.fetch_messages(mailbox_hash).await;
                                if let (Some(cache), Ok(ref msgs)) = (&cache, &result) {
                                    if let Err(e) =
                                        cache.save_messages(mh, msgs.clone()).await
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
            Message::SyncFoldersComplete(Err(e)) => {
                self.is_syncing = false;
                if self.folders.is_empty() {
                    self.status_message = format!("Failed to load folders: {}", e);
                } else {
                    self.status_message = format!(
                        "{} folders (sync failed: {})",
                        self.folders.len(),
                        e
                    );
                }
                log::error!("Folder sync failed: {}", e);
            }

            Message::SyncMessagesComplete(Ok(())) => {
                self.is_syncing = false;
                if let Some(idx) = self.selected_folder {
                    if let Some(folder) = self.folders.get(idx) {
                        let mailbox_hash = folder.mailbox_hash;
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            self.messages_offset = 0;
                            return cosmic::task::future(async move {
                                Message::CachedMessagesLoaded(
                                    cache
                                        .load_messages(mailbox_hash, DEFAULT_PAGE_SIZE, 0)
                                        .await,
                                )
                            });
                        }
                    }
                }
                self.status_message = format!("{} messages (synced)", self.messages.len());
            }
            Message::SyncMessagesComplete(Err(e)) => {
                self.is_syncing = false;
                self.status_message = format!("Sync failed: {}", e);
                log::error!("Message sync failed: {}", e);
            }

            Message::SelectFolder(index) => {
                self.selected_folder = Some(index);
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

                if let Some(folder) = self.folders.get(index) {
                    let mailbox_hash = folder.mailbox_hash;
                    let folder_name = folder.name.clone();
                    let mut tasks: Vec<Task<Message>> = Vec::new();

                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        tasks.push(cosmic::task::future(async move {
                            Message::CachedMessagesLoaded(
                                cache.load_messages(mailbox_hash, DEFAULT_PAGE_SIZE, 0).await,
                            )
                        }));
                    }

                    if let Some(session) = &self.session {
                        let session = session.clone();
                        let cache = self.cache.clone();
                        self.is_syncing = true;
                        self.status_message = format!("Loading {}...", folder_name);
                        let mbox_hash = MailboxHash(mailbox_hash);
                        tasks.push(cosmic::task::future(async move {
                            let result = session.fetch_messages(mbox_hash).await;
                            if let (Some(cache), Ok(ref msgs)) = (&cache, &result) {
                                if let Err(e) =
                                    cache.save_messages(mailbox_hash, msgs.clone()).await
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

            Message::LoadMoreMessages => {
                self.messages_offset += DEFAULT_PAGE_SIZE;
                let offset = self.messages_offset;

                if let Some(idx) = self.selected_folder {
                    if let Some(folder) = self.folders.get(idx) {
                        let mailbox_hash = folder.mailbox_hash;
                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            return cosmic::task::future(async move {
                                Message::CachedMessagesLoaded(
                                    cache
                                        .load_messages(mailbox_hash, DEFAULT_PAGE_SIZE, offset)
                                        .await,
                                )
                            });
                        }
                    }
                }
            }

            Message::Refresh => {
                if let Some(session) = &self.session {
                    let session = session.clone();
                    let cache = self.cache.clone();
                    self.is_syncing = true;
                    self.status_message = "Refreshing...".into();
                    return cosmic::task::future(async move {
                        let result = session.fetch_folders().await;
                        if let (Some(cache), Ok(ref folders)) = (&cache, &result) {
                            if let Err(e) = cache.save_folders(folders.clone()).await {
                                log::warn!("Failed to cache folders: {}", e);
                            }
                        }
                        Message::SyncFoldersComplete(result)
                    });
                }
            }

            _ => {}
        }
        Task::none()
    }

    /// Rebuild folder_map from current folders list.
    pub(super) fn rebuild_folder_map(&mut self) {
        self.folder_map.clear();
        for f in &self.folders {
            self.folder_map.insert(f.path.clone(), f.mailbox_hash);
        }
    }
}
