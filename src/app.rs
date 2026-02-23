use std::sync::Arc;

use cosmic::app::{Core, Task};
use cosmic::iced::Length;
use cosmic::widget;
use cosmic::Element;

use melib::{EnvelopeHash, MailboxHash};

use crate::config::Config;
use crate::core::imap::ImapSession;
use crate::core::models::{Folder, MessageSummary};
use crate::core::store::{CacheHandle, DEFAULT_PAGE_SIZE};

const APP_ID: &str = "com.cosmic_utils.email";

pub struct AppModel {
    core: Core,
    config: Config,

    session: Option<Arc<ImapSession>>,
    cache: Option<CacheHandle>,

    folders: Vec<Folder>,
    selected_folder: Option<usize>,

    messages: Vec<MessageSummary>,
    selected_message: Option<usize>,
    messages_offset: u32,
    has_more_messages: bool,

    preview_body: String,

    is_syncing: bool,
    status_message: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    Connected(Result<Arc<ImapSession>, String>),

    SelectFolder(usize),
    FoldersLoaded(Result<Vec<Folder>, String>),

    SelectMessage(usize),
    MessagesLoaded(Result<Vec<MessageSummary>, String>),

    BodyLoaded(Result<String, String>),

    // Cache-first messages
    CachedFoldersLoaded(Result<Vec<Folder>, String>),
    CachedMessagesLoaded(Result<Vec<MessageSummary>, String>),
    SyncFoldersComplete(Result<Vec<Folder>, String>),
    SyncMessagesComplete(Result<(), String>),
    LoadMoreMessages,

    OpenLink(String),
    Refresh,
    Noop,
}

impl cosmic::Application for AppModel {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = APP_ID;

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: Self::Flags) -> (Self, Task<Self::Message>) {
        let config = Config::from_env();

        // Open cache synchronously (just opens a file, fast)
        let cache = match CacheHandle::open() {
            Ok(c) => {
                log::info!("Cache opened successfully");
                Some(c)
            }
            Err(e) => {
                log::warn!("Failed to open cache, running without: {}", e);
                None
            }
        };

        let app = AppModel {
            core,
            config: config.clone(),
            session: None,
            cache: cache.clone(),
            folders: Vec::new(),
            selected_folder: None,
            messages: Vec::new(),
            selected_message: None,
            messages_offset: 0,
            has_more_messages: false,
            preview_body: String::new(),
            is_syncing: true,
            status_message: "Starting up...".into(),
        };

        let title_task = app.set_window_title("Nevermail".into());

        // Fire two parallel tasks: load cache + connect to IMAP
        let mut tasks = vec![title_task];

        // 1. Load cached folders (instant)
        if let Some(cache) = cache {
            tasks.push(cosmic::task::future(async move {
                Message::CachedFoldersLoaded(cache.load_folders().await)
            }));
        }

        // 2. Connect to IMAP (slow, runs in background)
        tasks.push(cosmic::task::future(async move {
            Message::Connected(ImapSession::connect(config).await)
        }));

        (app, cosmic::task::batch(tasks))
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let sidebar = crate::ui::sidebar::view(&self.folders, self.selected_folder);
        let message_list = crate::ui::message_list::view(
            &self.messages,
            self.selected_message,
            self.has_more_messages,
        );
        let message_view = crate::ui::message_view::view(&self.preview_body);

        let main_content = widget::row()
            .push(
                widget::container(sidebar)
                    .width(Length::FillPortion(1))
                    .height(Length::Fill),
            )
            .push(
                widget::container(message_list)
                    .width(Length::FillPortion(2))
                    .height(Length::Fill),
            )
            .push(
                widget::container(message_view)
                    .width(Length::FillPortion(3))
                    .height(Length::Fill),
            )
            .height(Length::Fill);

        let status_bar = widget::container(widget::text::caption(&self.status_message))
            .padding([4, 8])
            .width(Length::Fill);

        widget::column()
            .push(main_content)
            .push(status_bar)
            .height(Length::Fill)
            .into()
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            // -----------------------------------------------------------------
            // Cache-first: cached folders loaded at startup
            // -----------------------------------------------------------------
            Message::CachedFoldersLoaded(Ok(folders)) => {
                if !folders.is_empty() {
                    self.folders = folders;
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
                // Not fatal — IMAP connect will populate folders
            }

            // -----------------------------------------------------------------
            // Cache-first: cached messages loaded
            // -----------------------------------------------------------------
            Message::CachedMessagesLoaded(Ok(messages)) => {
                let count = messages.len();
                self.has_more_messages = count as u32 == DEFAULT_PAGE_SIZE;

                if self.messages_offset == 0 {
                    // First page: replace
                    self.messages = messages;
                } else {
                    // Subsequent pages: append
                    self.messages.extend(messages);
                }

                if !self.messages.is_empty() {
                    self.status_message =
                        format!("{} messages", self.messages.len());
                }
            }
            Message::CachedMessagesLoaded(Err(e)) => {
                log::warn!("Failed to load cached messages: {}", e);
            }

            // -----------------------------------------------------------------
            // IMAP connected — start background folder sync
            // -----------------------------------------------------------------
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
                    // Save to cache if available
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
                // Only show error if we have no cached data
                if self.folders.is_empty() {
                    self.status_message = format!("Connection failed: {}", e);
                } else {
                    self.status_message = format!(
                        "{} folders (offline — {})",
                        self.folders.len(),
                        e
                    );
                }
                log::error!("IMAP connection failed: {}", e);
            }

            // -----------------------------------------------------------------
            // Background folder sync complete
            // -----------------------------------------------------------------
            Message::SyncFoldersComplete(Ok(folders)) => {
                self.folders = folders;
                self.is_syncing = false;
                self.status_message = format!("{} folders", self.folders.len());

                // If no folder was selected yet, auto-select INBOX
                if self.selected_folder.is_none() {
                    if let Some(idx) = self.folders.iter().position(|f| f.path == "INBOX") {
                        self.selected_folder = Some(idx);
                    }
                }

                // Trigger background message sync for current folder
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

            // -----------------------------------------------------------------
            // Background message sync complete — reload from cache
            // -----------------------------------------------------------------
            Message::SyncMessagesComplete(Ok(())) => {
                self.is_syncing = false;
                // Reload page 1 from cache to pick up fresh data
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

            // -----------------------------------------------------------------
            // Legacy direct-from-server messages (used as fallback when no cache)
            // -----------------------------------------------------------------
            Message::FoldersLoaded(Ok(folders)) => {
                self.folders = folders;
                self.is_syncing = false;
                self.status_message = format!("{} folders loaded", self.folders.len());

                // Auto-select INBOX if present
                if let Some(idx) = self.folders.iter().position(|f| f.path == "INBOX") {
                    self.selected_folder = Some(idx);
                    let mailbox_hash = MailboxHash(self.folders[idx].mailbox_hash);
                    if let Some(session) = &self.session {
                        let session = session.clone();
                        self.is_syncing = true;
                        self.status_message = "Loading INBOX...".into();
                        return cosmic::task::future(async move {
                            Message::MessagesLoaded(
                                session.fetch_messages(mailbox_hash).await,
                            )
                        });
                    }
                }
            }
            Message::FoldersLoaded(Err(e)) => {
                self.is_syncing = false;
                self.status_message = format!("Failed to load folders: {}", e);
                log::error!("Folder fetch failed: {}", e);
            }

            // -----------------------------------------------------------------
            // Select folder — cache-first with background sync
            // -----------------------------------------------------------------
            Message::SelectFolder(index) => {
                self.selected_folder = Some(index);
                self.messages.clear();
                self.selected_message = None;
                self.preview_body.clear();
                self.messages_offset = 0;
                self.has_more_messages = false;

                if let Some(folder) = self.folders.get(index) {
                    let mailbox_hash = folder.mailbox_hash;
                    let folder_name = folder.name.clone();
                    let mut tasks: Vec<Task<Message>> = Vec::new();

                    // 1. Load from cache instantly
                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        tasks.push(cosmic::task::future(async move {
                            Message::CachedMessagesLoaded(
                                cache.load_messages(mailbox_hash, DEFAULT_PAGE_SIZE, 0).await,
                            )
                        }));
                    }

                    // 2. Background sync from server
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

            Message::MessagesLoaded(Ok(messages)) => {
                self.is_syncing = false;
                self.status_message = format!("{} messages", messages.len());
                self.messages = messages;
            }
            Message::MessagesLoaded(Err(e)) => {
                self.is_syncing = false;
                self.status_message = format!("Failed to load messages: {}", e);
                log::error!("Message fetch failed: {}", e);
            }

            // -----------------------------------------------------------------
            // Load more messages (pagination)
            // -----------------------------------------------------------------
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

            // -----------------------------------------------------------------
            // Select message — cache-first body loading
            // -----------------------------------------------------------------
            Message::SelectMessage(index) => {
                self.selected_message = Some(index);

                if let Some(msg) = self.messages.get(index) {
                    let envelope_hash = msg.envelope_hash;

                    // Try cache first
                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        let session = self.session.clone();
                        self.status_message = "Loading message...".into();
                        return cosmic::task::future(async move {
                            // Check cache
                            match cache.load_body(envelope_hash).await {
                                Ok(Some(body)) => Message::BodyLoaded(Ok(body)),
                                _ => {
                                    // Cache miss — fetch from server
                                    if let Some(session) = session {
                                        let result = session
                                            .fetch_body(EnvelopeHash(envelope_hash))
                                            .await;
                                        // Cache the result on success
                                        if let Ok(ref body) = result {
                                            if let Err(e) = cache
                                                .save_body(envelope_hash, body.clone())
                                                .await
                                            {
                                                log::warn!(
                                                    "Failed to cache body: {}",
                                                    e
                                                );
                                            }
                                        }
                                        Message::BodyLoaded(result)
                                    } else {
                                        Message::BodyLoaded(Err(
                                            "Not connected".to_string()
                                        ))
                                    }
                                }
                            }
                        });
                    }

                    // No cache — direct server fetch
                    if let Some(session) = &self.session {
                        let session = session.clone();
                        self.status_message = "Loading message...".into();
                        return cosmic::task::future(async move {
                            Message::BodyLoaded(
                                session.fetch_body(EnvelopeHash(envelope_hash)).await,
                            )
                        });
                    }
                }
            }

            Message::BodyLoaded(Ok(body)) => {
                self.preview_body = body;
                self.status_message = "Ready".into();
            }
            Message::BodyLoaded(Err(e)) => {
                self.preview_body = format!("Failed to load message body: {}", e);
                self.status_message = "Error loading message".into();
                log::error!("Body fetch failed: {}", e);
            }

            Message::OpenLink(url) => {
                crate::core::mime::open_link(&url);
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
            Message::Noop => {}
        }
        Task::none()
    }
}

impl AppModel {
    fn set_window_title(&self, title: String) -> cosmic::app::Task<Message> {
        self.core.set_title(self.core.main_window_id(), title)
    }
}
