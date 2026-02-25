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
use cosmic::widget::{image, markdown, pane_grid, text_editor};
use cosmic::Element;

use crate::config::{Config, ConfigNeedsInput, LayoutConfig};
use crate::core::imap::ImapSession;
use crate::core::models::{AttachmentData, DraggedFiles, Folder, MessageSummary};
use crate::core::store::CacheHandle;
use crate::ui::compose_dialog::ComposeMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    Sidebar,
    MessageList,
    MessageView,
}

const APP_ID: &str = "com.neverlight.email";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Syncing,
    Error(String),
}

pub struct AppModel {
    core: Core,
    pub(super) config: Option<Config>,

    pub(super) session: Option<Arc<ImapSession>>,
    pub(super) cache: Option<CacheHandle>,

    pub(super) folders: Vec<Folder>,
    pub(super) selected_folder: Option<usize>,

    pub(super) messages: Vec<MessageSummary>,
    pub(super) selected_message: Option<usize>,
    pub(super) messages_offset: u32,
    pub(super) has_more_messages: bool,

    pub(super) preview_body: String,
    pub(super) preview_markdown: Vec<markdown::Item>,
    pub(super) preview_attachments: Vec<AttachmentData>,
    pub(super) preview_image_handles: Vec<Option<image::Handle>>,

    /// Map folder paths (e.g. "Trash", "Archive") to mailbox hashes
    pub(super) folder_map: HashMap<String, u64>,

    /// Thread IDs that are currently collapsed (children hidden)
    pub(super) collapsed_threads: HashSet<u64>,
    /// Maps visible row positions → real indices into `messages`
    pub(super) visible_indices: Vec<usize>,
    /// Total messages per thread_id (for collapse indicators)
    pub(super) thread_sizes: HashMap<u64, usize>,

    pub(super) conn_state: ConnectionState,
    pub(super) status_message: String,

    // Search state
    pub(super) search_active: bool,
    pub(super) search_query: String,
    pub(super) search_focused: bool,

    // Compose dialog state
    pub(super) show_compose_dialog: bool,
    pub(super) compose_mode: ComposeMode,
    pub(super) compose_from: usize,
    pub(super) compose_to: String,
    pub(super) compose_subject: String,
    pub(super) compose_body: text_editor::Content,
    pub(super) compose_in_reply_to: Option<String>,
    pub(super) compose_references: Option<String>,
    pub(super) compose_attachments: Vec<AttachmentData>,
    pub(super) compose_error: Option<String>,
    pub(super) compose_drag_hover: bool,
    pub(super) is_sending: bool,

    // Setup dialog state
    pub(super) show_setup_dialog: bool,
    pub(super) password_only_mode: bool,
    pub(super) setup_server: String,
    pub(super) setup_port: String,
    pub(super) setup_username: String,
    pub(super) setup_password: String,
    pub(super) setup_starttls: bool,
    pub(super) setup_password_visible: bool,
    pub(super) setup_email_addresses: String,
    pub(super) setup_error: Option<String>,

    // DnD state
    pub(super) folder_drag_target: Option<usize>,

    // Pane layout
    pub(super) panes: pane_grid::State<PaneKind>,
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
    ComposeAttach,
    ComposeAttachLoaded(Result<Vec<AttachmentData>, String>),
    ComposeRemoveAttachment(usize),
    ComposeFilesDropped(DraggedFiles),
    ComposeFileTransfer(String),
    ComposeFileTransferResolved(Result<Vec<String>, String>),
    ComposeDragEnter,
    ComposeDragLeave,
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

    // Message-to-folder drag
    DragMessageToFolder {
        envelope_hash: u64,
        source_mailbox: u64,
        dest_mailbox: u64,
    },
    FolderDragEnter(usize),
    FolderDragLeave,

    PaneResized(pane_grid::ResizeEvent),

    ForceReconnect,
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

        let layout = LayoutConfig::load();
        let pane_config = pane_grid::Configuration::Split {
            axis: pane_grid::Axis::Vertical,
            ratio: layout.sidebar_ratio,
            a: Box::new(pane_grid::Configuration::Pane(PaneKind::Sidebar)),
            b: Box::new(pane_grid::Configuration::Split {
                axis: pane_grid::Axis::Vertical,
                ratio: layout.list_ratio,
                a: Box::new(pane_grid::Configuration::Pane(PaneKind::MessageList)),
                b: Box::new(pane_grid::Configuration::Pane(PaneKind::MessageView)),
            }),
        };
        let panes = pane_grid::State::with_configuration(pane_config);

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
            conn_state: ConnectionState::Disconnected,
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
            compose_attachments: Vec::new(),
            compose_error: None,
            compose_drag_hover: false,
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

            folder_drag_target: None,

            panes,
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
                app.conn_state = ConnectionState::Connecting;
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
                &self.compose_attachments,
                self.compose_error.as_deref(),
                self.is_sending,
                self.compose_drag_hover,
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
        let main_content = widget::PaneGrid::new(&self.panes, |_pane, kind, _is_maximized| {
            let body: Element<'_, Self::Message> = match kind {
                PaneKind::Sidebar => crate::ui::sidebar::view(
                    &self.folders,
                    self.selected_folder,
                    &self.conn_state,
                    self.folder_drag_target,
                ),
                PaneKind::MessageList => crate::ui::message_list::view(
                    &self.messages,
                    &self.visible_indices,
                    self.selected_message,
                    self.has_more_messages && !self.search_active,
                    &self.collapsed_threads,
                    &self.thread_sizes,
                    self.search_active,
                    &self.search_query,
                ),
                PaneKind::MessageView => {
                    let selected_msg = self.selected_message.and_then(|i| {
                        self.messages.get(i).map(|msg| (i, msg))
                    });
                    crate::ui::message_view::view(
                        &self.preview_markdown,
                        selected_msg,
                        &self.preview_attachments,
                        &self.preview_image_handles,
                    )
                }
            };
            pane_grid::Content::new(body)
        })
        .on_resize(10.0, Message::PaneResized)
        .width(Length::Fill)
        .height(Length::Fill);

        let status_bar = widget::container(widget::text::caption(&self.status_message))
            .padding([4, 8])
            .width(Length::Fill);

        let content: Element<'_, Self::Message> = widget::column()
            .push(main_content)
            .push(status_bar)
            .height(Length::Fill)
            .into();

        // WARNING: DO NOT move this into dialog(). COSMIC dialog overlays don't
        // register drag_destinations with the Wayland compositor — dnd_destination
        // widgets inside dialogs are invisible to the compositor and drops silently
        // fail (files snap back to file manager). This must live in view().
        //
        // Two codepaths:
        //   on_file_transfer → portal key → ashpd resolve → paths (Wayland native)
        //   on_finish         → text/uri-list → url parse   → paths (X11 fallback)
        widget::dnd_destination::dnd_destination_for_data::<DraggedFiles, _>(
            content,
            |data, _action| match data {
                Some(files) => Message::ComposeFilesDropped(files),
                None => Message::Noop,
            },
        )
        .on_file_transfer(Message::ComposeFileTransfer)
        .on_enter(|_x, _y, _mimes| Message::ComposeDragEnter)
        .on_leave(|| Message::ComposeDragLeave)
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
            | Message::ComposeAttach
            | Message::ComposeAttachLoaded(_)
            | Message::ComposeRemoveAttachment(_)
            | Message::ComposeFilesDropped(_)
            | Message::ComposeFileTransfer(_)
            | Message::ComposeFileTransferResolved(_)
            | Message::ComposeDragEnter
            | Message::ComposeDragLeave
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
            | Message::ForceReconnect
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
            | Message::DragMessageToFolder { .. }
            | Message::FolderDragEnter(_)
            | Message::FolderDragLeave
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

            // Pane layout
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => {
                self.panes.resize(split, ratio);
                self.save_layout();
                Task::none()
            }
            Message::Noop => Task::none(),
        }
    }
}

impl AppModel {
    fn set_window_title(&self, title: String) -> cosmic::app::Task<Message> {
        self.core.set_title(self.core.main_window_id(), title)
    }

    pub(super) fn is_busy(&self) -> bool {
        matches!(
            self.conn_state,
            ConnectionState::Connecting | ConnectionState::Syncing
        )
    }

    /// Dispatch a message through the update loop (for recursive calls from handlers).
    pub(super) fn dispatch(&mut self, message: Message) -> Task<Message> {
        <Self as cosmic::Application>::update(self, message)
    }

    /// Extract current split ratios from pane_grid layout tree and persist.
    fn save_layout(&self) {
        fn extract_ratios(node: &pane_grid::Node) -> (f32, f32) {
            match node {
                pane_grid::Node::Split { ratio, a, b, .. } => {
                    let sidebar_ratio = *ratio;
                    // Inner split is in the 'b' branch
                    let list_ratio = match b.as_ref() {
                        pane_grid::Node::Split { ratio, .. } => *ratio,
                        _ => 0.40,
                    };
                    // If 'a' is also a split (shouldn't be, but be safe), recurse
                    let _ = a;
                    (sidebar_ratio, list_ratio)
                }
                _ => (0.15, 0.40),
            }
        }

        let (sidebar_ratio, list_ratio) = extract_ratios(self.panes.layout());
        let layout = LayoutConfig {
            sidebar_ratio,
            list_ratio,
        };
        layout.save();
    }
}
