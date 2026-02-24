use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use cosmic::app::{Core, Task};
use cosmic::iced::keyboard;
use cosmic::iced::{Event, Length, Subscription};
use cosmic::widget;
use cosmic::widget::{image, markdown, text_editor};
use cosmic::Element;

use futures::{SinkExt, StreamExt};
use melib::backends::{BackendEvent, FlagOp, RefreshEventKind};
use melib::email::Flag;
use melib::{EnvelopeHash, MailboxHash};

use crate::config::{Config, ConfigNeedsInput, FileConfig, PasswordBackend, SmtpConfig};
use crate::core::imap::ImapSession;
use crate::core::models::{AttachmentData, Folder, MessageSummary};
use crate::core::smtp::{self, OutgoingEmail};
use crate::core::store::{self, CacheHandle, DEFAULT_PAGE_SIZE};
use crate::ui::compose_dialog::ComposeMode;

const APP_ID: &str = "com.cosmic_utils.email";

pub struct AppModel {
    core: Core,
    config: Option<Config>,

    session: Option<Arc<ImapSession>>,
    cache: Option<CacheHandle>,

    folders: Vec<Folder>,
    selected_folder: Option<usize>,

    messages: Vec<MessageSummary>,
    selected_message: Option<usize>,
    messages_offset: u32,
    has_more_messages: bool,

    preview_body: String,
    preview_markdown: Vec<markdown::Item>,
    preview_attachments: Vec<AttachmentData>,
    preview_image_handles: Vec<Option<image::Handle>>,

    /// Map folder paths (e.g. "Trash", "Archive") to mailbox hashes
    folder_map: HashMap<String, u64>,

    /// Thread IDs that are currently collapsed (children hidden)
    collapsed_threads: HashSet<u64>,
    /// Maps visible row positions → real indices into `messages`
    visible_indices: Vec<usize>,
    /// Total messages per thread_id (for collapse indicators)
    thread_sizes: HashMap<u64, usize>,

    is_syncing: bool,
    status_message: String,

    // Search state
    search_active: bool,
    search_query: String,
    search_focused: bool,

    // Compose dialog state
    show_compose_dialog: bool,
    compose_mode: ComposeMode,
    compose_from: usize,
    compose_to: String,
    compose_subject: String,
    compose_body: text_editor::Content,
    compose_in_reply_to: Option<String>,
    compose_references: Option<String>,
    compose_error: Option<String>,
    is_sending: bool,

    // Setup dialog state
    show_setup_dialog: bool,
    password_only_mode: bool,
    setup_server: String,
    setup_port: String,
    setup_username: String,
    setup_password: String,
    setup_starttls: bool,
    setup_password_visible: bool,
    setup_email_addresses: String,
    setup_error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Connected(Result<Arc<ImapSession>, String>),

    SelectFolder(usize),

    SelectMessage(usize),
    BodyLoaded(Result<(String, String, Vec<AttachmentData>), String>),
    LinkClicked(markdown::Url),
    CopyBody,

    SaveAttachment(usize),
    SaveAttachmentComplete(Result<String, String>),

    // Cache-first messages
    CachedFoldersLoaded(Result<Vec<Folder>, String>),
    CachedMessagesLoaded(Result<Vec<MessageSummary>, String>),
    SyncFoldersComplete(Result<Vec<Folder>, String>),
    SyncMessagesComplete(Result<(), String>),
    LoadMoreMessages,

    // Flag/move actions
    ToggleRead(usize),
    ToggleStar(usize),
    TrashMessage(usize),
    ArchiveMessage(usize),
    FlagOpComplete {
        envelope_hash: u64,
        result: Result<u8, String>,
    },
    MoveOpComplete {
        envelope_hash: u64,
        result: Result<(), String>,
    },

    // Keyboard navigation
    SelectionUp,
    SelectionDown,
    ActivateSelection,
    ToggleThreadCollapse,

    // Compose messages
    ComposeNew,
    ComposeReply,
    ComposeForward,
    ComposeFromChanged(usize),
    ComposeToChanged(String),
    ComposeSubjectChanged(String),
    ComposeBodyAction(text_editor::Action),
    ComposeSend,
    ComposeCancel,
    SendComplete(Result<(), String>),

    ImapEvent(ImapWatchEvent),

    // Search
    SearchActivate,
    SearchQueryChanged(String),
    SearchExecute,
    SearchResultsLoaded(Result<Vec<MessageSummary>, String>),
    SearchClear,

    Refresh,
    Noop,

    // Setup dialog messages
    SetupServerChanged(String),
    SetupPortChanged(String),
    SetupUsernameChanged(String),
    SetupPasswordChanged(String),
    SetupStarttlsToggled(bool),
    SetupPasswordVisibilityToggled,
    SetupEmailAddressesChanged(String),
    SetupSubmit,
    SetupCancel,
}

#[derive(Debug, Clone)]
pub enum ImapWatchEvent {
    NewMessage {
        mailbox_hash: u64,
        subject: String,
        from: String,
    },
    MessageRemoved {
        mailbox_hash: u64,
        envelope_hash: u64,
    },
    FlagsChanged {
        mailbox_hash: u64,
        envelope_hash: u64,
        flags: u8,
    },
    Rescan,
    WatchError(String),
    WatchEnded,
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

        let mut app = AppModel {
            core,
            config: None,
            session: None,
            cache: cache.clone(),
            folders: Vec::new(),
            selected_folder: None,
            messages: Vec::new(),
            selected_message: None,
            messages_offset: 0,
            has_more_messages: false,
            preview_body: String::new(),
            preview_markdown: Vec::new(),
            preview_attachments: Vec::new(),
            preview_image_handles: Vec::new(),
            folder_map: HashMap::new(),
            collapsed_threads: HashSet::new(),
            visible_indices: Vec::new(),
            thread_sizes: HashMap::new(),
            is_syncing: false,
            status_message: "Starting up...".into(),

            search_active: false,
            search_query: String::new(),
            search_focused: false,

            show_compose_dialog: false,
            compose_mode: ComposeMode::New,
            compose_from: 0,
            compose_to: String::new(),
            compose_subject: String::new(),
            compose_body: text_editor::Content::new(),
            compose_in_reply_to: None,
            compose_references: None,
            compose_error: None,
            is_sending: false,

            show_setup_dialog: false,
            password_only_mode: false,
            setup_server: String::new(),
            setup_port: "993".into(),
            setup_username: String::new(),
            setup_password: String::new(),
            setup_starttls: false,
            setup_password_visible: false,
            setup_email_addresses: String::new(),
            setup_error: None,
        };

        let title_task = app.set_window_title("Nevermail".into());
        let mut tasks = vec![title_task];

        // Load cached folders regardless of config state
        if let Some(cache) = cache.clone() {
            tasks.push(cosmic::task::future(async move {
                Message::CachedFoldersLoaded(cache.load_folders().await)
            }));
        }

        // Resolve config: env → file+keyring → show dialog
        match Config::resolve() {
            Ok(config) => {
                app.config = Some(config.clone());
                app.is_syncing = true;
                tasks.push(cosmic::task::future(async move {
                    Message::Connected(ImapSession::connect(config).await)
                }));
            }
            Err(ConfigNeedsInput::FullSetup) => {
                app.show_setup_dialog = true;
                app.password_only_mode = false;
                app.status_message = "Setup required — enter your account details".into();
            }
            Err(ConfigNeedsInput::PasswordOnly {
                server,
                port,
                username,
                starttls,
                error,
            }) => {
                app.show_setup_dialog = true;
                app.password_only_mode = true;
                app.setup_server = server;
                app.setup_port = port.to_string();
                app.setup_username = username;
                app.setup_starttls = starttls;
                app.setup_error = error;
                app.status_message = "Password required".into();
            }
        }

        (app, cosmic::task::batch(tasks))
    }

    fn dialog(&self) -> Option<Element<'_, Self::Message>> {
        if self.show_setup_dialog {
            return Some(self.setup_dialog());
        }
        if self.show_compose_dialog {
            let addrs = self
                .config
                .as_ref()
                .map(|c| c.email_addresses.as_slice())
                .unwrap_or(&[]);
            return Some(crate::ui::compose_dialog::view(
                &self.compose_mode,
                addrs,
                self.compose_from,
                &self.compose_to,
                &self.compose_subject,
                &self.compose_body,
                self.compose_error.as_deref(),
                self.is_sending,
            ));
        }
        None
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subs = Vec::new();

        if self.search_focused {
            // When search input has focus, only intercept Escape.
            // All other keys must reach the text_input widget unimpeded.
            subs.push(cosmic::iced_futures::event::listen_raw(|event, status, _| {
                if cosmic::iced_core::event::Status::Ignored != status {
                    return None;
                }
                match event {
                    Event::Keyboard(keyboard::Event::KeyPressed { key, .. }) => match key {
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::SearchClear)
                        }
                        _ => None,
                    },
                    _ => None,
                }
            }));
        } else {
            // Full keyboard shortcuts when not typing in search
            subs.push(cosmic::iced_futures::event::listen_raw(|event, status, _| {
                if cosmic::iced_core::event::Status::Ignored != status {
                    return None;
                }
                match event {
                    Event::Keyboard(keyboard::Event::KeyPressed {
                        key, modifiers, ..
                    }) => match key {
                        keyboard::Key::Named(keyboard::key::Named::ArrowDown) => {
                            Some(Message::SelectionDown)
                        }
                        keyboard::Key::Named(keyboard::key::Named::ArrowUp) => {
                            Some(Message::SelectionUp)
                        }
                        keyboard::Key::Named(keyboard::key::Named::Enter) => {
                            Some(Message::ActivateSelection)
                        }
                        keyboard::Key::Character(ref c)
                            if c.as_str() == "/" && !modifiers.control() =>
                        {
                            Some(Message::SearchActivate)
                        }
                        keyboard::Key::Character(ref c)
                            if c.as_str() == "j" && !modifiers.control() =>
                        {
                            Some(Message::SelectionDown)
                        }
                        keyboard::Key::Character(ref c)
                            if c.as_str() == "k" && !modifiers.control() =>
                        {
                            Some(Message::SelectionUp)
                        }
                        keyboard::Key::Character(ref c) if c.as_str() == " " => {
                            Some(Message::ToggleThreadCollapse)
                        }
                        keyboard::Key::Character(ref c)
                            if c.as_str() == "c" && !modifiers.control() =>
                        {
                            Some(Message::ComposeNew)
                        }
                        keyboard::Key::Character(ref c)
                            if c.as_str() == "r" && !modifiers.control() =>
                        {
                            Some(Message::ComposeReply)
                        }
                        keyboard::Key::Character(ref c)
                            if c.as_str() == "f" && !modifiers.control() =>
                        {
                            Some(Message::ComposeForward)
                        }
                        keyboard::Key::Named(keyboard::key::Named::Escape) => {
                            Some(Message::SearchClear)
                        }
                        _ => None,
                    },
                    _ => None,
                }
            }));
        }

        if let Some(session) = &self.session {
            let session = session.clone();
            subs.push(
                Subscription::run_with_id("imap-watch", imap_watch_stream(session))
                    .map(Message::ImapEvent),
            );
        }

        Subscription::batch(subs)
    }

    fn view(&self) -> Element<'_, Self::Message> {
        let sidebar = crate::ui::sidebar::view(&self.folders, self.selected_folder);
        let message_list = crate::ui::message_list::view(
            &self.messages,
            &self.visible_indices,
            self.selected_message,
            self.has_more_messages && !self.search_active,
            &self.collapsed_threads,
            &self.thread_sizes,
            self.search_active,
            &self.search_query,
        );
        let selected_msg = self.selected_message.and_then(|i| {
            self.messages.get(i).map(|msg| (i, msg))
        });
        let message_view = crate::ui::message_view::view(
            &self.preview_markdown,
            selected_msg,
            &self.preview_attachments,
            &self.preview_image_handles,
        );

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
            // Compose dialog handlers
            // -----------------------------------------------------------------
            Message::ComposeNew => {
                if self.show_setup_dialog || self.show_compose_dialog {
                    return Task::none();
                }
                self.compose_mode = ComposeMode::New;
                self.compose_from = 0;
                self.compose_to.clear();
                self.compose_subject.clear();
                self.compose_body = text_editor::Content::new();
                self.compose_in_reply_to = None;
                self.compose_references = None;
                self.compose_error = None;
                self.is_sending = false;
                self.show_compose_dialog = true;
            }

            Message::ComposeReply => {
                if self.show_setup_dialog || self.show_compose_dialog {
                    return Task::none();
                }
                if let Some(index) = self.selected_message {
                    if let Some(msg) = self.messages.get(index) {
                        self.compose_mode = ComposeMode::Reply;
                        self.compose_to = msg.from.clone();

                        let subj = &msg.subject;
                        self.compose_subject = if subj.starts_with("Re: ") {
                            subj.clone()
                        } else {
                            format!("Re: {subj}")
                        };

                        let quoted = quote_body(&self.preview_body, &msg.from, &msg.date);
                        self.compose_body = text_editor::Content::with_text(&format!("\n\n{quoted}"));

                        self.compose_in_reply_to = Some(msg.message_id.clone());
                        self.compose_references = Some(build_references(
                            msg.in_reply_to.as_deref(),
                            &msg.message_id,
                        ));
                        self.compose_error = None;
                        self.is_sending = false;
                        self.show_compose_dialog = true;
                    }
                }
            }

            Message::ComposeForward => {
                if self.show_setup_dialog || self.show_compose_dialog {
                    return Task::none();
                }
                if let Some(index) = self.selected_message {
                    if let Some(msg) = self.messages.get(index) {
                        self.compose_mode = ComposeMode::Forward;
                        self.compose_to.clear();

                        let subj = &msg.subject;
                        self.compose_subject = if subj.starts_with("Fwd: ") {
                            subj.clone()
                        } else {
                            format!("Fwd: {subj}")
                        };

                        let fwd = forward_body(
                            &self.preview_body,
                            &msg.from,
                            &msg.date,
                            &msg.subject,
                        );
                        self.compose_body = text_editor::Content::with_text(&format!("\n\n{fwd}"));

                        self.compose_in_reply_to = None;
                        self.compose_references = None;
                        self.compose_error = None;
                        self.is_sending = false;
                        self.show_compose_dialog = true;
                    }
                }
            }

            Message::ComposeFromChanged(i) => {
                self.compose_from = i;
            }
            Message::ComposeToChanged(v) => {
                self.compose_to = v;
            }
            Message::ComposeSubjectChanged(v) => {
                self.compose_subject = v;
            }
            Message::ComposeBodyAction(action) => {
                self.compose_body.perform(action);
            }

            Message::ComposeSend => {
                if self.compose_to.trim().is_empty() {
                    self.compose_error = Some("Recipient is required".into());
                    return Task::none();
                }

                let body_text = self.compose_body.text();
                if body_text.trim().is_empty() {
                    self.compose_error = Some("Message body is required".into());
                    return Task::none();
                }

                let Some(ref config) = self.config else {
                    self.compose_error = Some("Not configured".into());
                    return Task::none();
                };

                let from_addr = config
                    .email_addresses
                    .get(self.compose_from)
                    .cloned()
                    .unwrap_or_else(|| {
                        config.email_addresses.first().cloned().unwrap_or_default()
                    });
                if from_addr.is_empty() {
                    self.compose_error = Some(
                        "No email address configured. Re-run setup to add one.".into(),
                    );
                    return Task::none();
                }

                self.is_sending = true;
                self.compose_error = None;

                let smtp_config = SmtpConfig::from_imap_config(config);
                let email = OutgoingEmail {
                    from: from_addr,
                    to: self.compose_to.clone(),
                    subject: self.compose_subject.clone(),
                    body: body_text,
                    in_reply_to: self.compose_in_reply_to.clone(),
                    references: self.compose_references.clone(),
                };

                return cosmic::task::future(async move {
                    Message::SendComplete(smtp::send_email(&smtp_config, &email).await)
                });
            }

            Message::ComposeCancel => {
                self.show_compose_dialog = false;
                self.is_sending = false;
            }

            Message::SendComplete(Ok(())) => {
                self.show_compose_dialog = false;
                self.is_sending = false;
                self.compose_to.clear();
                self.compose_subject.clear();
                self.compose_body = text_editor::Content::new();
                self.compose_in_reply_to = None;
                self.compose_references = None;
                self.compose_error = None;
                self.status_message = "Message sent".into();
            }

            Message::SendComplete(Err(e)) => {
                self.is_sending = false;
                self.compose_error = Some(format!("Send failed: {e}"));
            }

            // -----------------------------------------------------------------
            // Setup dialog input handlers
            // -----------------------------------------------------------------
            Message::SetupServerChanged(v) => {
                self.setup_server = v;
            }
            Message::SetupPortChanged(v) => {
                self.setup_port = v;
            }
            Message::SetupUsernameChanged(v) => {
                self.setup_username = v;
            }
            Message::SetupPasswordChanged(v) => {
                self.setup_password = v;
            }
            Message::SetupStarttlsToggled(v) => {
                self.setup_starttls = v;
            }
            Message::SetupPasswordVisibilityToggled => {
                self.setup_password_visible = !self.setup_password_visible;
            }
            Message::SetupEmailAddressesChanged(v) => {
                self.setup_email_addresses = v;
            }

            // -----------------------------------------------------------------
            // Setup submit — validate, store credentials, connect
            // -----------------------------------------------------------------
            Message::SetupSubmit => {
                // Validate
                if self.setup_server.trim().is_empty()
                    || self.setup_username.trim().is_empty()
                    || self.setup_password.is_empty()
                {
                    self.setup_error = Some("All fields are required".into());
                    return Task::none();
                }
                let port: u16 = match self.setup_port.trim().parse() {
                    Ok(p) => p,
                    Err(_) => {
                        self.setup_error = Some("Port must be a number (e.g. 993)".into());
                        return Task::none();
                    }
                };

                let email_addresses: Vec<String> = self
                    .setup_email_addresses
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !self.password_only_mode && email_addresses.is_empty() {
                    self.setup_error =
                        Some("At least one email address is required for sending".into());
                    return Task::none();
                }

                let server = self.setup_server.trim().to_string();
                let username = self.setup_username.trim().to_string();
                let password = self.setup_password.clone();
                let starttls = self.setup_starttls;

                // Try keyring first; fall back to plaintext on failure
                let password_backend =
                    match crate::core::keyring::set_password(&username, &server, &password) {
                        Ok(()) => {
                            log::info!("Password stored in keyring");
                            PasswordBackend::Keyring
                        }
                        Err(e) => {
                            log::warn!("Keyring unavailable ({}), using plaintext", e);
                            PasswordBackend::Plaintext {
                                value: password.clone(),
                            }
                        }
                    };

                // Save config file
                let fc = FileConfig {
                    server: server.clone(),
                    port,
                    username: username.clone(),
                    starttls,
                    password: password_backend,
                    email_addresses: email_addresses.clone(),
                };
                if let Err(e) = fc.save() {
                    log::error!("Failed to save config: {}", e);
                    self.setup_error = Some(format!("Failed to save config: {e}"));
                    return Task::none();
                }

                // Build runtime config and connect
                let config = Config {
                    imap_server: server,
                    imap_port: port,
                    username,
                    password,
                    use_starttls: starttls,
                    email_addresses,
                };

                self.config = Some(config.clone());
                self.show_setup_dialog = false;
                self.setup_password.clear();
                self.setup_error = None;
                self.is_syncing = true;
                self.status_message = "Connecting...".into();

                return cosmic::task::future(async move {
                    Message::Connected(ImapSession::connect(config).await)
                });
            }

            // -----------------------------------------------------------------
            // Setup cancel — browse offline or show empty
            // -----------------------------------------------------------------
            Message::SetupCancel => {
                self.show_setup_dialog = false;
                if self.folders.is_empty() {
                    self.status_message = "Not connected — no cached data".into();
                } else {
                    self.status_message =
                        format!("{} folders (offline)", self.folders.len());
                }
            }

            // -----------------------------------------------------------------
            // Cache-first: cached folders loaded at startup
            // -----------------------------------------------------------------
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

            // -----------------------------------------------------------------
            // Cache-first: cached messages loaded
            // -----------------------------------------------------------------
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

            // -----------------------------------------------------------------
            // Background folder sync complete
            // -----------------------------------------------------------------
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

            // -----------------------------------------------------------------
            // Background message sync complete — reload from cache
            // -----------------------------------------------------------------
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

            // -----------------------------------------------------------------
            // Select folder — cache-first with background sync
            // -----------------------------------------------------------------
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

                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        let session = self.session.clone();
                        self.status_message = "Loading message...".into();
                        return cosmic::task::future(async move {
                            // Unified cache-first: try cache (includes attachments)
                            if let Ok(Some((md_body, plain_body, attachments))) =
                                cache.load_body(envelope_hash).await
                            {
                                return Message::BodyLoaded(Ok((md_body, plain_body, attachments)));
                            }

                            // Cache miss: fetch from IMAP, save to cache
                            if let Some(session) = session {
                                let result = session
                                    .fetch_body(EnvelopeHash(envelope_hash))
                                    .await;
                                if let Ok((ref md_body, ref plain_body, ref attachments)) = result {
                                    if let Err(e) = cache
                                        .save_body(
                                            envelope_hash,
                                            md_body.clone(),
                                            plain_body.clone(),
                                            attachments.clone(),
                                        )
                                        .await
                                    {
                                        log::warn!("Failed to cache body: {}", e);
                                    }
                                }
                                Message::BodyLoaded(result)
                            } else {
                                Message::BodyLoaded(Err(
                                    "Not connected".to_string()
                                ))
                            }
                        });
                    }

                    // No-cache fallback: direct IMAP fetch
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

            Message::BodyLoaded(Ok((markdown_body, plain_body, attachments))) => {
                // Safety net: if clean_email_html still produces too many items
                // (the markdown widget has no virtualization), fall back to plain text.
                const MAX_MD_ITEMS: usize = 200;

                let items: Vec<markdown::Item> = markdown::parse(&markdown_body).collect();
                log::debug!(
                    "Markdown: {} bytes input, {} items parsed",
                    markdown_body.len(),
                    items.len()
                );

                if items.len() <= MAX_MD_ITEMS {
                    self.preview_markdown = items;
                } else {
                    log::warn!(
                        "Markdown items ({}) exceed cap ({}), falling back to plain text",
                        items.len(),
                        MAX_MD_ITEMS
                    );
                    // Plain text through markdown::parse produces ~1 item per paragraph
                    self.preview_markdown = markdown::parse(&plain_body).collect();
                }
                self.preview_body = plain_body;
                self.preview_image_handles = attachments
                    .iter()
                    .map(|a| {
                        if a.is_image() {
                            Some(image::Handle::from_bytes(a.data.clone()))
                        } else {
                            None
                        }
                    })
                    .collect();
                self.preview_attachments = attachments;
                self.status_message = "Ready".into();
            }
            Message::BodyLoaded(Err(e)) => {
                let msg = format!("Failed to load message body: {}", e);
                self.preview_markdown = markdown::parse(&msg).collect();
                self.preview_body = msg;
                self.status_message = "Error loading message".into();
                log::error!("Body fetch failed: {}", e);
            }

            Message::LinkClicked(url) => {
                crate::core::mime::open_link(url.as_str());
            }

            Message::CopyBody => {
                if !self.preview_body.is_empty() {
                    return cosmic::iced::clipboard::write(self.preview_body.clone());
                }
            }

            Message::SaveAttachment(index) => {
                if let Some(att) = self.preview_attachments.get(index) {
                    let filename = att.filename.clone();
                    let data = att.data.clone();
                    return cosmic::task::future(async move {
                        let dir = dirs::download_dir()
                            .unwrap_or_else(|| PathBuf::from("."));
                        let path = dir.join(&filename);
                        match tokio::fs::write(&path, &data).await {
                            Ok(()) => Message::SaveAttachmentComplete(
                                Ok(path.display().to_string()),
                            ),
                            Err(e) => Message::SaveAttachmentComplete(
                                Err(format!("Save failed: {e}")),
                            ),
                        }
                    });
                }
            }

            Message::SaveAttachmentComplete(Ok(path)) => {
                self.status_message = format!("Saved to {path}");
            }
            Message::SaveAttachmentComplete(Err(e)) => {
                self.status_message = e;
                log::error!("Attachment save failed: {}", self.status_message);
            }

            // -----------------------------------------------------------------
            // Flag actions — optimistic UI + background IMAP op
            // -----------------------------------------------------------------
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

                    if let Some(session) = &self.session {
                        let session = session.clone();
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

                    if let Some(session) = &self.session {
                        let session = session.clone();
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

            Message::TrashMessage(index) => {
                if let Some(trash_hash) = self.folder_map.get("Trash").or_else(|| self.folder_map.get("INBOX.Trash")).copied() {
                    if let Some(msg) = self.messages.get(index) {
                        let envelope_hash = msg.envelope_hash;
                        let source_mailbox = msg.mailbox_hash;

                        // Optimistic: remove from list
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

                        let mut tasks: Vec<Task<Message>> = Vec::new();

                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let new_flags = store::flags_to_u8(true, false);
                            tasks.push(cosmic::task::future(async move {
                                if let Err(e) = cache.update_flags(envelope_hash, new_flags, format!("move:{}", trash_hash)).await {
                                    log::warn!("Failed to update cache for trash: {}", e);
                                }
                                Message::Noop
                            }));
                        }

                        if let Some(session) = &self.session {
                            let session = session.clone();
                            tasks.push(cosmic::task::future(async move {
                                let result = session
                                    .move_messages(
                                        EnvelopeHash(envelope_hash),
                                        MailboxHash(source_mailbox),
                                        MailboxHash(trash_hash),
                                    )
                                    .await;
                                Message::MoveOpComplete {
                                    envelope_hash,
                                    result,
                                }
                            }));
                        }

                        if !tasks.is_empty() {
                            return cosmic::task::batch(tasks);
                        }
                    }
                } else {
                    self.status_message = "Trash folder not found".into();
                }
            }

            Message::ArchiveMessage(index) => {
                if let Some(archive_hash) = self.folder_map.get("Archive").or_else(|| self.folder_map.get("INBOX.Archive")).copied() {
                    if let Some(msg) = self.messages.get(index) {
                        let envelope_hash = msg.envelope_hash;
                        let source_mailbox = msg.mailbox_hash;

                        // Optimistic: remove from list
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

                        let mut tasks: Vec<Task<Message>> = Vec::new();

                        if let Some(cache) = &self.cache {
                            let cache = cache.clone();
                            let new_flags = store::flags_to_u8(true, false);
                            tasks.push(cosmic::task::future(async move {
                                if let Err(e) = cache.update_flags(envelope_hash, new_flags, format!("move:{}", archive_hash)).await {
                                    log::warn!("Failed to update cache for archive: {}", e);
                                }
                                Message::Noop
                            }));
                        }

                        if let Some(session) = &self.session {
                            let session = session.clone();
                            tasks.push(cosmic::task::future(async move {
                                let result = session
                                    .move_messages(
                                        EnvelopeHash(envelope_hash),
                                        MailboxHash(source_mailbox),
                                        MailboxHash(archive_hash),
                                    )
                                    .await;
                                Message::MoveOpComplete {
                                    envelope_hash,
                                    result,
                                }
                            }));
                        }

                        if !tasks.is_empty() {
                            return cosmic::task::batch(tasks);
                        }
                    }
                } else {
                    self.status_message = "Archive folder not found".into();
                }
            }

            // -----------------------------------------------------------------
            // Background flag/move op results
            // -----------------------------------------------------------------
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
                        // TODO: re-insert message on failure (would need to store removed msg)
                        // For now, a refresh will restore correct state
                    }
                }
            }

            // -----------------------------------------------------------------
            // Keyboard navigation
            // -----------------------------------------------------------------
            Message::SelectionDown => {
                if self.messages.is_empty() {
                    return Task::none();
                }
                let current_vis_pos = self
                    .selected_message
                    .and_then(|sel| self.visible_indices.iter().position(|&ri| ri == sel));
                let new_vis_pos = match current_vis_pos {
                    Some(pos) => (pos + 1).min(self.visible_indices.len().saturating_sub(1)),
                    None => 0,
                };
                if let Some(&real_index) = self.visible_indices.get(new_vis_pos) {
                    self.selected_message = Some(real_index);
                    return self.update(Message::SelectMessage(real_index));
                }
            }

            Message::SelectionUp => {
                if self.messages.is_empty() {
                    return Task::none();
                }
                let current_vis_pos = self
                    .selected_message
                    .and_then(|sel| self.visible_indices.iter().position(|&ri| ri == sel));
                let new_vis_pos = match current_vis_pos {
                    Some(pos) => pos.saturating_sub(1),
                    None => 0,
                };
                if let Some(&real_index) = self.visible_indices.get(new_vis_pos) {
                    self.selected_message = Some(real_index);
                    return self.update(Message::SelectMessage(real_index));
                }
            }

            Message::ActivateSelection => {
                if let Some(index) = self.selected_message {
                    return self.update(Message::SelectMessage(index));
                }
            }

            Message::ToggleThreadCollapse => {
                if let Some(index) = self.selected_message {
                    if let Some(msg) = self.messages.get(index) {
                        if let Some(tid) = msg.thread_id {
                            let size = self.thread_sizes.get(&tid).copied().unwrap_or(1);
                            if size > 1 {
                                if self.collapsed_threads.contains(&tid) {
                                    // Expand
                                    self.collapsed_threads.remove(&tid);
                                } else {
                                    // Collapse — if selected message is a child, jump to root
                                    self.collapsed_threads.insert(tid);
                                    if msg.thread_depth > 0 {
                                        // Find the thread root (first message with this thread_id and depth 0)
                                        if let Some(root_idx) = self.messages.iter().position(|m| {
                                            m.thread_id == Some(tid) && m.thread_depth == 0
                                        }) {
                                            self.selected_message = Some(root_idx);
                                        }
                                    }
                                }
                                self.recompute_visible();
                            }
                        }
                    }
                }
            }

            // -----------------------------------------------------------------
            // Search
            // -----------------------------------------------------------------
            Message::SearchActivate => {
                if self.show_setup_dialog || self.show_compose_dialog || self.search_focused {
                    return Task::none();
                }
                self.search_active = true;
                self.search_focused = true;
                self.search_query.clear();
                return widget::text_input::focus(
                    crate::ui::message_list::search_input_id(),
                );
            }
            Message::SearchQueryChanged(q) => {
                self.search_query = q;
            }
            Message::SearchExecute => {
                let query = self.search_query.trim().to_string();
                if query.is_empty() {
                    return Task::none();
                }
                if let Some(cache) = &self.cache {
                    let cache = cache.clone();
                    self.status_message = "Searching...".into();
                    return cosmic::task::future(async move {
                        Message::SearchResultsLoaded(cache.search(query).await)
                    });
                }
            }
            Message::SearchResultsLoaded(Ok(results)) => {
                let count = results.len();
                let query = self.search_query.clone();
                self.messages = results;
                self.selected_message = None;
                self.preview_body.clear();
                self.preview_markdown.clear();
                self.preview_attachments.clear();
                self.preview_image_handles.clear();
                self.collapsed_threads.clear();
                self.has_more_messages = false;
                self.recompute_visible();
                self.search_focused = false;
                if count > 0 {
                    self.status_message = format!("Search: {} results for \"{}\"", count, query);
                } else {
                    self.status_message = format!("Search: no results for \"{}\"", query);
                }
            }
            Message::SearchResultsLoaded(Err(e)) => {
                self.search_focused = false;
                self.status_message = format!("Search failed: {}", e);
                log::error!("Search failed: {}", e);
            }
            Message::SearchClear => {
                if self.search_active {
                    self.search_active = false;
                    self.search_focused = false;
                    self.search_query.clear();
                    // Restore previous folder view
                    if let Some(idx) = self.selected_folder {
                        return self.update(Message::SelectFolder(idx));
                    }
                } else {
                    // Not searching — Escape cancels compose dialog
                    self.show_compose_dialog = false;
                    self.is_sending = false;
                }
            }

            // -----------------------------------------------------------------
            // IMAP watch events (new mail notifications)
            // -----------------------------------------------------------------
            Message::ImapEvent(ImapWatchEvent::NewMessage {
                mailbox_hash,
                subject,
                from,
            }) => {
                let notif_task = cosmic::task::future(async move {
                    let _ = tokio::task::spawn_blocking(move || {
                        let _ = notify_rust::Notification::new()
                            .summary(&format!("From: {}", from))
                            .body(&subject)
                            .icon("mail-message-new")
                            .timeout(5000)
                            .show();
                    })
                    .await;
                    Message::Noop
                });

                if let Some(idx) = self.selected_folder {
                    if let Some(folder) = self.folders.get(idx) {
                        if folder.mailbox_hash == mailbox_hash {
                            let refresh_task = self.update(Message::Refresh);
                            return cosmic::task::batch(vec![notif_task, refresh_task]);
                        }
                    }
                }
                return notif_task;
            }
            Message::ImapEvent(ImapWatchEvent::MessageRemoved {
                mailbox_hash,
                envelope_hash,
            }) => {
                // Only act if we're viewing the affected mailbox
                let viewing_mailbox = self.selected_folder.and_then(|i| self.folders.get(i))
                    .map_or(false, |f| f.mailbox_hash == mailbox_hash);

                if viewing_mailbox {
                    // Find and remove from messages list
                    if let Some(pos) = self.messages.iter().position(|m| m.envelope_hash == envelope_hash) {
                        self.messages.remove(pos);

                        // Adjust selection
                        match self.selected_message {
                            Some(sel) if sel == pos => {
                                // Selected message was removed — clear preview
                                self.selected_message = if self.messages.is_empty() {
                                    None
                                } else {
                                    Some(sel.min(self.messages.len() - 1))
                                };
                                self.preview_body.clear();
                                self.preview_markdown.clear();
                                self.preview_attachments.clear();
                                self.preview_image_handles.clear();
                            }
                            Some(sel) if sel > pos => {
                                self.selected_message = Some(sel - 1);
                            }
                            _ => {}
                        }

                        self.recompute_visible();
                    }

                    // Fire-and-forget cache cleanup
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
            }

            Message::ImapEvent(ImapWatchEvent::FlagsChanged {
                mailbox_hash,
                envelope_hash,
                flags,
            }) => {
                let viewing_mailbox = self.selected_folder.and_then(|i| self.folders.get(i))
                    .map_or(false, |f| f.mailbox_hash == mailbox_hash);

                if viewing_mailbox {
                    let (is_read, is_starred) = store::flags_from_u8(flags);
                    if let Some(msg) = self.messages.iter_mut()
                        .find(|m| m.envelope_hash == envelope_hash)
                    {
                        msg.is_read = is_read;
                        msg.is_starred = is_starred;
                    }

                    // Sync server flags and clear any pending op in cache
                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        return cosmic::task::future(async move {
                            if let Err(e) = cache.clear_pending_op(envelope_hash, flags).await {
                                log::warn!("Failed to sync flags in cache: {}", e);
                            }
                            Message::Noop
                        });
                    }
                }
            }

            Message::ImapEvent(ImapWatchEvent::Rescan) => {
                return self.update(Message::Refresh);
            }

            Message::ImapEvent(ImapWatchEvent::WatchError(e)) => {
                log::warn!("IMAP watch error: {}", e);
            }
            Message::ImapEvent(ImapWatchEvent::WatchEnded) => {
                log::info!("IMAP watch stream ended");
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

    fn setup_dialog(&self) -> Element<'_, Message> {
        let mut controls = widget::column().spacing(12);

        if !self.password_only_mode {
            controls = controls
                .push(
                    widget::text_input("mail.example.com", &self.setup_server)
                        .label("IMAP Server")
                        .on_input(Message::SetupServerChanged),
                )
                .push(
                    widget::text_input("993", &self.setup_port)
                        .label("Port")
                        .on_input(Message::SetupPortChanged),
                )
                .push(
                    widget::text_input("you@example.com", &self.setup_username)
                        .label("Username")
                        .on_input(Message::SetupUsernameChanged),
                );
        }

        controls = controls.push(
            widget::text_input::secure_input(
                "Password",
                &self.setup_password,
                Some(Message::SetupPasswordVisibilityToggled),
                !self.setup_password_visible,
            )
            .label("Password")
            .on_input(Message::SetupPasswordChanged),
        );

        if !self.password_only_mode {
            controls = controls
                .push(
                    widget::text_input("you@example.com, alias@example.com", &self.setup_email_addresses)
                        .label("Email addresses (comma-separated)")
                        .on_input(Message::SetupEmailAddressesChanged),
                )
                .push(
                    widget::settings::item::builder("Use STARTTLS")
                        .toggler(self.setup_starttls, Message::SetupStarttlsToggled),
                );
        }

        let mut dialog = widget::dialog()
            .title(if self.password_only_mode {
                "Enter Password"
            } else {
                "Account Setup"
            })
            .control(controls)
            .primary_action(
                widget::button::suggested("Connect").on_press(Message::SetupSubmit),
            )
            .secondary_action(
                widget::button::standard("Cancel").on_press(Message::SetupCancel),
            );

        if let Some(ref err) = self.setup_error {
            dialog = dialog.body(err.as_str());
        }

        dialog.into()
    }

    /// Rebuild `visible_indices` and `thread_sizes` based on current messages
    /// and collapsed state.
    fn recompute_visible(&mut self) {
        // Rebuild thread_sizes
        self.thread_sizes.clear();
        for msg in &self.messages {
            if let Some(tid) = msg.thread_id {
                *self.thread_sizes.entry(tid).or_insert(0) += 1;
            }
        }

        // Rebuild visible_indices: hide children of collapsed threads
        self.visible_indices.clear();
        for (i, msg) in self.messages.iter().enumerate() {
            if msg.thread_depth > 0 {
                if let Some(tid) = msg.thread_id {
                    if self.collapsed_threads.contains(&tid) {
                        continue; // hidden child
                    }
                }
            }
            self.visible_indices.push(i);
        }
    }

    /// Rebuild folder_map from current folders list.
    fn rebuild_folder_map(&mut self) {
        self.folder_map.clear();
        for f in &self.folders {
            self.folder_map.insert(f.path.clone(), f.mailbox_hash);
        }
    }
}

fn quote_body(body: &str, from: &str, date: &str) -> String {
    let mut out = format!("On {date}, {from} wrote:\n");
    for line in body.lines() {
        out.push_str("> ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn forward_body(body: &str, from: &str, date: &str, subject: &str) -> String {
    let mut out = String::from("---------- Forwarded message ----------\n");
    out.push_str(&format!("From: {from}\n"));
    out.push_str(&format!("Date: {date}\n"));
    out.push_str(&format!("Subject: {subject}\n\n"));
    out.push_str(body);
    out
}

fn build_references(in_reply_to: Option<&str>, message_id: &str) -> String {
    match in_reply_to {
        Some(irt) => format!("{irt} {message_id}"),
        None => message_id.to_string(),
    }
}

fn imap_watch_stream(
    session: Arc<ImapSession>,
) -> impl futures::Stream<Item = ImapWatchEvent> {
    cosmic::iced_futures::stream::channel(50, move |mut output| async move {
        match session.watch().await {
            Ok(stream) => {
                let mut stream = std::pin::pin!(stream);
                while let Some(event) = stream.next().await {
                    match event {
                        Ok(BackendEvent::Refresh(rev)) => {
                            match rev.kind {
                                RefreshEventKind::Create(envelope) => {
                                    let from = envelope
                                        .from()
                                        .iter()
                                        .map(|a| a.to_string())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    let _ = output
                                        .send(ImapWatchEvent::NewMessage {
                                            mailbox_hash: rev.mailbox_hash.0,
                                            subject: envelope.subject().to_string(),
                                            from,
                                        })
                                        .await;
                                }
                                RefreshEventKind::Remove(envelope_hash) => {
                                    let _ = output
                                        .send(ImapWatchEvent::MessageRemoved {
                                            mailbox_hash: rev.mailbox_hash.0,
                                            envelope_hash: envelope_hash.0,
                                        })
                                        .await;
                                }
                                RefreshEventKind::NewFlags(envelope_hash, (flag, _tags)) => {
                                    let is_read = flag.contains(Flag::SEEN);
                                    let is_starred = flag.contains(Flag::FLAGGED);
                                    let flags = store::flags_to_u8(is_read, is_starred);
                                    let _ = output
                                        .send(ImapWatchEvent::FlagsChanged {
                                            mailbox_hash: rev.mailbox_hash.0,
                                            envelope_hash: envelope_hash.0,
                                            flags,
                                        })
                                        .await;
                                }
                                RefreshEventKind::Rescan => {
                                    let _ = output
                                        .send(ImapWatchEvent::Rescan)
                                        .await;
                                }
                                other => {
                                    log::debug!(
                                        "Unhandled IMAP watch event kind: {:?}",
                                        other
                                    );
                                }
                            }
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = output
                                .send(ImapWatchEvent::WatchError(e.to_string()))
                                .await;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = output.send(ImapWatchEvent::WatchError(e)).await;
            }
        }
        let _ = output.send(ImapWatchEvent::WatchEnded).await;
    })
}
