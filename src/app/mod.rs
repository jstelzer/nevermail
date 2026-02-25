mod actions;
mod body;
mod compose;
mod navigation;
mod search;
mod setup;
mod sync;
mod watch;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use cosmic::app::{Core, Task};
use cosmic::iced::keyboard;
use cosmic::iced::{Event, Length, Subscription};
use cosmic::widget;
use cosmic::widget::{image, markdown, text_editor};
use cosmic::Element;

use crate::config::{Config, ConfigNeedsInput};
use crate::core::imap::ImapSession;
use crate::core::models::{AttachmentData, Folder, MessageSummary};
use crate::core::store::CacheHandle;
use crate::ui::compose_dialog::ComposeMode;

const APP_ID: &str = "com.neverlight.email";

pub struct AppModel {
    core: Core,
    pub(crate) config: Option<Config>,

    pub(crate) session: Option<Arc<ImapSession>>,
    pub(crate) cache: Option<CacheHandle>,

    pub(crate) folders: Vec<Folder>,
    pub(crate) selected_folder: Option<usize>,

    pub(crate) messages: Vec<MessageSummary>,
    pub(crate) selected_message: Option<usize>,
    pub(crate) messages_offset: u32,
    pub(crate) has_more_messages: bool,

    pub(crate) preview_body: String,
    pub(crate) preview_markdown: Vec<markdown::Item>,
    pub(crate) preview_attachments: Vec<AttachmentData>,
    pub(crate) preview_image_handles: Vec<Option<image::Handle>>,

    /// Map folder paths (e.g. "Trash", "Archive") to mailbox hashes
    pub(crate) folder_map: HashMap<String, u64>,

    /// Thread IDs that are currently collapsed (children hidden)
    pub(crate) collapsed_threads: HashSet<u64>,
    /// Maps visible row positions → real indices into `messages`
    pub(crate) visible_indices: Vec<usize>,
    /// Total messages per thread_id (for collapse indicators)
    pub(crate) thread_sizes: HashMap<u64, usize>,

    pub(crate) is_syncing: bool,
    pub(crate) status_message: String,

    // Search state
    pub(crate) search_active: bool,
    pub(crate) search_query: String,
    pub(crate) search_focused: bool,

    // Compose dialog state
    pub(crate) show_compose_dialog: bool,
    pub(crate) compose_mode: ComposeMode,
    pub(crate) compose_from: usize,
    pub(crate) compose_to: String,
    pub(crate) compose_subject: String,
    pub(crate) compose_body: text_editor::Content,
    pub(crate) compose_in_reply_to: Option<String>,
    pub(crate) compose_references: Option<String>,
    pub(crate) compose_error: Option<String>,
    pub(crate) is_sending: bool,

    // Setup dialog state
    pub(crate) show_setup_dialog: bool,
    pub(crate) password_only_mode: bool,
    pub(crate) setup_server: String,
    pub(crate) setup_port: String,
    pub(crate) setup_username: String,
    pub(crate) setup_password: String,
    pub(crate) setup_starttls: bool,
    pub(crate) setup_password_visible: bool,
    pub(crate) setup_email_addresses: String,
    pub(crate) setup_error: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Message {
    Connected(Result<Arc<ImapSession>, String>),

    SelectFolder(usize),

    ViewBody(usize),
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
    Trash(usize),
    Archive(usize),
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
                    Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        ..
                    }) => Some(Message::SearchClear),
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
                Subscription::run_with_id("imap-watch", watch::imap_watch_stream(session))
                    .map(Message::ImapEvent),
            );

            // Periodic full sync to catch changes IDLE misses (e.g. remote deletions)
            subs.push(Subscription::run_with_id(
                "periodic-sync",
                cosmic::iced_futures::stream::channel(1, |mut output| async move {
                    use futures::SinkExt;
                    let mut interval =
                        tokio::time::interval(std::time::Duration::from_secs(5 * 60));
                    interval.tick().await; // skip immediate first tick
                    loop {
                        interval.tick().await;
                        let _ = output.send(Message::Refresh).await;
                    }
                }),
            ));
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
            // Compose
            Message::ComposeNew
            | Message::ComposeReply
            | Message::ComposeForward
            | Message::ComposeFromChanged(_)
            | Message::ComposeToChanged(_)
            | Message::ComposeSubjectChanged(_)
            | Message::ComposeBodyAction(_)
            | Message::ComposeSend
            | Message::ComposeCancel
            | Message::SendComplete(_) => self.handle_compose(message),

            // Setup
            Message::SetupServerChanged(_)
            | Message::SetupPortChanged(_)
            | Message::SetupUsernameChanged(_)
            | Message::SetupPasswordChanged(_)
            | Message::SetupStarttlsToggled(_)
            | Message::SetupPasswordVisibilityToggled
            | Message::SetupEmailAddressesChanged(_)
            | Message::SetupSubmit
            | Message::SetupCancel => self.handle_setup(message),

            // Sync / connection / folder selection
            Message::Connected(_)
            | Message::CachedFoldersLoaded(_)
            | Message::CachedMessagesLoaded(_)
            | Message::SyncFoldersComplete(_)
            | Message::SyncMessagesComplete(_)
            | Message::SelectFolder(_)
            | Message::LoadMoreMessages
            | Message::Refresh => self.handle_sync(message),

            // Body / attachment viewing
            Message::ViewBody(_)
            | Message::BodyLoaded(_)
            | Message::LinkClicked(_)
            | Message::CopyBody
            | Message::SaveAttachment(_)
            | Message::SaveAttachmentComplete(_) => self.handle_body(message),

            // Flag / move actions
            Message::ToggleRead(_)
            | Message::ToggleStar(_)
            | Message::Trash(_)
            | Message::Archive(_)
            | Message::FlagOpComplete { .. }
            | Message::MoveOpComplete { .. } => self.handle_actions(message),

            // Keyboard navigation
            Message::SelectionUp
            | Message::SelectionDown
            | Message::ActivateSelection
            | Message::ToggleThreadCollapse => self.handle_navigation(message),

            // Search
            Message::SearchActivate
            | Message::SearchQueryChanged(_)
            | Message::SearchExecute
            | Message::SearchResultsLoaded(_)
            | Message::SearchClear => self.handle_search(message),

            // IMAP watch events
            Message::ImapEvent(_) => self.handle_watch(message),

            Message::Noop => Task::none(),
        }
    }
}

impl AppModel {
    fn set_window_title(&self, title: String) -> cosmic::app::Task<Message> {
        self.core.set_title(self.core.main_window_id(), title)
    }

    /// Dispatch a message through the update loop (for recursive calls from handlers).
    pub(crate) fn dispatch(&mut self, message: Message) -> Task<Message> {
        <Self as cosmic::Application>::update(self, message)
    }
}
