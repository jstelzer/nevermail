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

use neverlight_mail_core::config::{AccountConfig, AccountId, ConfigNeedsInput, LayoutConfig};
use neverlight_mail_core::setup::SetupModel;
use neverlight_mail_core::imap::ImapSession;
use neverlight_mail_core::models::{AttachmentData, Folder, MessageSummary};
use neverlight_mail_core::store::CacheHandle;

use crate::dnd_models::DraggedFiles;
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

// ---------------------------------------------------------------------------
// Per-account state
// ---------------------------------------------------------------------------

pub struct AccountState {
    pub config: AccountConfig,
    pub session: Option<Arc<ImapSession>>,
    pub conn_state: ConnectionState,
    pub folders: Vec<Folder>,
    pub folder_map: HashMap<String, u64>,
    pub collapsed: bool,
}

impl AccountState {
    pub fn new(config: AccountConfig) -> Self {
        AccountState {
            config,
            session: None,
            conn_state: ConnectionState::Disconnected,
            folders: Vec::new(),
            folder_map: HashMap::new(),
            collapsed: false,
        }
    }

    pub fn rebuild_folder_map(&mut self) {
        self.folder_map.clear();
        for f in &self.folders {
            self.folder_map.insert(f.path.clone(), f.mailbox_hash);
        }
    }
}

// ---------------------------------------------------------------------------
// AppModel
// ---------------------------------------------------------------------------

pub struct AppModel {
    core: Core,

    // Multi-account state
    pub(super) accounts: Vec<AccountState>,
    pub(super) active_account: Option<usize>,

    pub(super) cache: Option<CacheHandle>,

    pub(super) selected_folder: Option<usize>,

    pub(super) messages: Vec<MessageSummary>,
    pub(super) selected_message: Option<usize>,
    pub(super) messages_offset: u32,
    pub(super) has_more_messages: bool,

    pub(super) preview_body: String,
    pub(super) preview_markdown: Vec<markdown::Item>,
    pub(super) preview_attachments: Vec<AttachmentData>,
    pub(super) preview_image_handles: Vec<Option<image::Handle>>,

    /// Thread IDs that are currently collapsed (children hidden)
    pub(super) collapsed_threads: HashSet<u64>,
    /// Maps visible row positions → real indices into `messages`
    pub(super) visible_indices: Vec<usize>,
    /// Total messages per thread_id (for collapse indicators)
    pub(super) thread_sizes: HashMap<u64, usize>,
    /// Snapshot of optimistically removed messages for move rollback.
    pub(super) pending_move_restore: HashMap<u64, (MessageSummary, usize)>,

    pub(super) status_message: String,

    // Search state
    pub(super) search_active: bool,
    pub(super) search_query: String,
    pub(super) search_focused: bool,

    // Compose dialog state
    pub(super) show_compose_dialog: bool,
    pub(super) compose_mode: ComposeMode,
    pub(super) compose_account: usize,
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
    // Cached for dialog() lifetime (updated when compose_account changes)
    pub(super) compose_account_labels: Vec<String>,
    pub(super) compose_cached_from: Vec<String>,

    // Setup dialog state — core fields live in SetupModel, visibility is local
    pub(super) setup_model: Option<SetupModel>,
    pub(super) setup_password_visible: bool,

    // DnD state
    pub(super) folder_drag_target: Option<usize>,

    /// Body view deferred until IMAP session is ready
    pub(super) pending_body: Option<usize>,

    /// Auto-mark-read: suppressed when user manually toggles back to unread
    pub(super) auto_read_suppressed: bool,

    // Pane layout
    pub(super) panes: pane_grid::State<PaneKind>,
}

#[derive(Debug, Clone)]
pub enum Message {
    AccountConnected {
        account_id: AccountId,
        result: Result<Arc<ImapSession>, String>,
    },

    SelectFolder(usize, usize), // (account_idx, folder_idx)

    ViewBody(usize),
    BodyDeferred,
    BodyLoaded(Result<(String, String, Vec<AttachmentData>), String>),
    LinkClicked(markdown::Url),
    CopyBody,

    SaveAttachment(usize),
    SaveAttachmentComplete(Result<String, String>),

    // Cache-first messages
    CachedFoldersLoaded {
        account_id: AccountId,
        result: Result<Vec<Folder>, String>,
    },
    CachedMessagesLoaded(Result<Vec<MessageSummary>, String>),
    SyncFoldersComplete {
        account_id: AccountId,
        result: Result<Vec<Folder>, String>,
    },
    SyncMessagesComplete(Result<(), String>),
    LoadMoreMessages,

    // Flag/move actions
    ToggleRead(usize),
    ToggleStar(usize),
    Trash(usize),
    Archive(usize),
    FlagOpComplete {
        envelope_hash: u64,
        prev_flags: u8,
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
    ComposeAccountChanged(usize),
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

    ImapEvent(AccountId, ImapWatchEvent),

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

    /// Auto-mark-read: fires 5s after a message is displayed
    AutoMarkRead(u64),

    ForceReconnect(AccountId),
    Refresh,
    Noop,

    // Account management
    AccountAdd,
    AccountEdit(AccountId),
    AccountRemove(AccountId),
    ToggleAccountCollapse(usize),

    // Setup dialog messages
    SetupLabelChanged(String),
    SetupServerChanged(String),
    SetupPortChanged(String),
    SetupUsernameChanged(String),
    SetupPasswordChanged(String),
    SetupStarttlsToggled(bool),
    SetupPasswordVisibilityToggled,
    SetupEmailAddressesChanged(String),
    SetupSmtpServerChanged(String),
    SetupSmtpPortChanged(String),
    SetupSmtpUsernameChanged(String),
    SetupSmtpPasswordChanged(String),
    SetupSmtpStarttlsToggled(bool),
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
            accounts: Vec::new(),
            active_account: None,
            cache: cache.clone(),
            selected_folder: None,
            messages: Vec::new(),
            selected_message: None,
            messages_offset: 0,
            has_more_messages: false,
            preview_body: String::new(),
            preview_markdown: Vec::new(),
            preview_attachments: Vec::new(),
            preview_image_handles: Vec::new(),
            collapsed_threads: HashSet::new(),
            visible_indices: Vec::new(),
            thread_sizes: HashMap::new(),
            pending_move_restore: HashMap::new(),
            status_message: "Starting up...".into(),

            search_active: false,
            search_query: String::new(),
            search_focused: false,

            show_compose_dialog: false,
            compose_mode: ComposeMode::New,
            compose_account: 0,
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
            compose_account_labels: Vec::new(),
            compose_cached_from: Vec::new(),

            setup_model: None,
            setup_password_visible: false,

            folder_drag_target: None,
            pending_body: None,
            auto_read_suppressed: false,

            panes,
        };

        let title_task = app.set_window_title("Nevermail".into());
        let mut tasks = vec![title_task];

        // Resolve config: env → file+keyring → show dialog
        match neverlight_mail_core::config::Config::resolve_all_accounts() {
            Ok(account_configs) => {
                for ac in account_configs {
                    let account_id = ac.id.clone();
                    let imap_config = ac.to_imap_config();
                    let mut acct = AccountState::new(ac);
                    acct.conn_state = ConnectionState::Connecting;
                    app.accounts.push(acct);

                    // Load cached folders for this account
                    if let Some(cache) = cache.clone() {
                        let aid = account_id.clone();
                        tasks.push(cosmic::task::future(async move {
                            let result = cache.load_folders(aid.clone()).await;
                            Message::CachedFoldersLoaded { account_id: aid, result }
                        }));
                    }

                    // Start connecting
                    let aid = account_id.clone();
                    tasks.push(cosmic::task::future(async move {
                        let result = ImapSession::connect(imap_config).await;
                        Message::AccountConnected { account_id: aid, result }
                    }));
                }
                if app.accounts.is_empty() {
                    app.setup_model = Some(SetupModel::from_config_needs(&ConfigNeedsInput::FullSetup));
                    app.status_message = "Setup required — enter your account details".into();
                }
            }
            Err(ref needs) => {
                let status = match needs {
                    ConfigNeedsInput::FullSetup => "Setup required — enter your account details",
                    ConfigNeedsInput::PasswordOnly { .. } => "Password required",
                };
                app.setup_model = Some(SetupModel::from_config_needs(needs));
                app.status_message = status.into();
            }
        }

        (app, cosmic::task::batch(tasks))
    }

    fn dialog(&self) -> Option<Element<'_, Self::Message>> {
        if self.setup_model.is_some() {
            return Some(self.setup_dialog());
        }
        if self.show_compose_dialog {
            return Some(crate::ui::compose_dialog::view(
                crate::ui::compose_dialog::ComposeViewState {
                    mode: &self.compose_mode,
                    account_labels: &self.compose_account_labels,
                    selected_account: self.compose_account,
                    from_addresses: &self.compose_cached_from,
                    from_selected: self.compose_from,
                    to: &self.compose_to,
                    subject: &self.compose_subject,
                    body: &self.compose_body,
                    attachments: &self.compose_attachments,
                    error: self.compose_error.as_deref(),
                    is_sending: self.is_sending,
                    drag_hover: self.compose_drag_hover,
                },
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

        // Per-account IMAP watch streams
        for (i, acct) in self.accounts.iter().enumerate() {
            if let Some(session) = &acct.session {
                let session = session.clone();
                let account_id = acct.config.id.clone();
                let sub_id = format!("imap-watch-{}", i);
                subs.push(
                    Subscription::run_with_id(sub_id, watch::imap_watch_stream(session))
                        .map(move |evt| Message::ImapEvent(account_id.clone(), evt)),
                );
            }
        }

        // Periodic full sync (any connected account)
        let has_any_session = self.accounts.iter().any(|a| a.session.is_some());
        if has_any_session {
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
                    &self.accounts,
                    self.active_account,
                    self.selected_folder,
                    self.folder_drag_target,
                ),
                PaneKind::MessageList => crate::ui::message_list::view(
                    crate::ui::message_list::MessageListState {
                        messages: &self.messages,
                        visible_indices: &self.visible_indices,
                        selected: self.selected_message,
                        has_more: self.has_more_messages && !self.search_active,
                        collapsed_threads: &self.collapsed_threads,
                        thread_sizes: &self.thread_sizes,
                        search_active: self.search_active,
                        search_query: &self.search_query,
                    },
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
            | Message::ComposeAccountChanged(_)
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
            Message::SetupLabelChanged(_)
            | Message::SetupServerChanged(_)
            | Message::SetupPortChanged(_)
            | Message::SetupUsernameChanged(_)
            | Message::SetupPasswordChanged(_)
            | Message::SetupStarttlsToggled(_)
            | Message::SetupPasswordVisibilityToggled
            | Message::SetupEmailAddressesChanged(_)
            | Message::SetupSmtpServerChanged(_)
            | Message::SetupSmtpPortChanged(_)
            | Message::SetupSmtpUsernameChanged(_)
            | Message::SetupSmtpPasswordChanged(_)
            | Message::SetupSmtpStarttlsToggled(_)
            | Message::SetupSubmit
            | Message::SetupCancel => self.handle_setup(message),

            // Account management
            Message::AccountAdd
            | Message::AccountEdit(_)
            | Message::AccountRemove(_)
            | Message::ToggleAccountCollapse(_) => self.handle_account_management(message),

            // Sync / connection / folder selection
            Message::AccountConnected { .. }
            | Message::CachedFoldersLoaded { .. }
            | Message::CachedMessagesLoaded(_)
            | Message::SyncFoldersComplete { .. }
            | Message::SyncMessagesComplete(_)
            | Message::SelectFolder(_, _)
            | Message::LoadMoreMessages
            | Message::ForceReconnect(_)
            | Message::Refresh => self.handle_sync(message),

            // Body / attachment viewing
            Message::ViewBody(_)
            | Message::BodyDeferred
            | Message::BodyLoaded(_)
            | Message::LinkClicked(_)
            | Message::CopyBody
            | Message::SaveAttachment(_)
            | Message::SaveAttachmentComplete(_) => self.handle_body(message),

            // Flag / move actions
            Message::ToggleRead(_)
            | Message::ToggleStar(_)
            | Message::AutoMarkRead(_)
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
            Message::ImapEvent(_, _) => self.handle_watch(message),

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
        self.active_account
            .and_then(|i| self.accounts.get(i))
            .is_some_and(|a| {
                matches!(
                    a.conn_state,
                    ConnectionState::Connecting | ConnectionState::Syncing
                )
            })
    }

    /// Dispatch a message through the update loop (for recursive calls from handlers).
    pub(super) fn dispatch(&mut self, message: Message) -> Task<Message> {
        <Self as cosmic::Application>::update(self, message)
    }

    /// Find the account index that owns a given mailbox_hash.
    pub(super) fn account_for_mailbox(&self, mailbox_hash: u64) -> Option<usize> {
        self.accounts.iter().position(|a| {
            a.folders.iter().any(|f| f.mailbox_hash == mailbox_hash)
        })
    }

    /// Get the session for a given mailbox_hash.
    pub(super) fn session_for_mailbox(&self, mailbox_hash: u64) -> Option<Arc<ImapSession>> {
        self.account_for_mailbox(mailbox_hash)
            .and_then(|i| self.accounts[i].session.clone())
    }

    /// Get the folder_map for a given mailbox_hash's owning account.
    pub(super) fn folder_map_for_mailbox(&self, mailbox_hash: u64) -> Option<&HashMap<String, u64>> {
        self.account_for_mailbox(mailbox_hash)
            .map(|i| &self.accounts[i].folder_map)
    }

    /// Get the active account's ID, or empty string.
    pub(super) fn active_account_id(&self) -> String {
        self.active_account
            .and_then(|i| self.accounts.get(i))
            .map(|a| a.config.id.clone())
            .unwrap_or_default()
    }

    /// Get the active account's session.
    pub(super) fn active_session(&self) -> Option<Arc<ImapSession>> {
        self.active_account
            .and_then(|i| self.accounts.get(i))
            .and_then(|a| a.session.clone())
    }

    /// Find account index by ID.
    pub(super) fn account_index(&self, account_id: &str) -> Option<usize> {
        self.accounts.iter().position(|a| a.config.id == account_id)
    }

    /// Refresh the cached compose labels (account labels + from addresses)
    /// so dialog() can borrow them with &self lifetime.
    pub(super) fn refresh_compose_cache(&mut self) {
        self.compose_account_labels = self.accounts.iter().map(|a| a.config.label.clone()).collect();
        self.compose_cached_from = self
            .accounts
            .get(self.compose_account)
            .map(|a| a.config.email_addresses.clone())
            .unwrap_or_default();
    }

    /// Handle account management messages (add/edit/remove/collapse).
    fn handle_account_management(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::AccountAdd => {
                self.setup_model = Some(SetupModel::from_config_needs(&ConfigNeedsInput::FullSetup));
                self.setup_password_visible = false;
            }
            Message::AccountEdit(ref id) => {
                if let Some(acct) = self.accounts.iter().find(|a| &a.config.id == id) {
                    use neverlight_mail_core::setup::SetupFields;
                    self.setup_model = Some(SetupModel::for_edit(
                        id.clone(),
                        SetupFields {
                            label: acct.config.label.clone(),
                            server: acct.config.imap_server.clone(),
                            port: acct.config.imap_port.to_string(),
                            username: acct.config.username.clone(),
                            email: acct.config.email_addresses.join(", "),
                            starttls: acct.config.use_starttls,
                            smtp_server: acct.config.smtp_overrides.server.clone().unwrap_or_default(),
                            smtp_port: acct.config.smtp_overrides.port.map(|p| p.to_string()).unwrap_or_else(|| "587".into()),
                            smtp_username: acct.config.smtp_overrides.username.clone().unwrap_or_default(),
                            smtp_starttls: acct.config.smtp_overrides.use_starttls.unwrap_or(true),
                        },
                    ));
                    self.setup_password_visible = false;
                }
            }
            Message::AccountRemove(ref id) => {
                if let Some(idx) = self.account_index(id) {
                    let removed_id = self.accounts[idx].config.id.clone();
                    let removed_username = self.accounts[idx].config.username.clone();
                    let removed_server = self.accounts[idx].config.imap_server.clone();
                    self.accounts.remove(idx);
                    // Adjust active_account
                    if let Some(active) = self.active_account {
                        if active == idx {
                            self.active_account = None;
                            self.messages.clear();
                            self.selected_folder = None;
                            self.preview_body.clear();
                            self.preview_markdown.clear();
                        } else if active > idx {
                            self.active_account = Some(active - 1);
                        }
                    }
                    // Save updated config
                    let _ = self.save_multi_account_config();

                    // Clean up keyring passwords
                    if let Err(e) = neverlight_mail_core::keyring::delete_password(&removed_username, &removed_server) {
                        log::warn!("Failed to delete IMAP password from keyring: {}", e);
                    }
                    if let Err(e) = neverlight_mail_core::keyring::delete_smtp_password(&removed_id) {
                        log::debug!("No SMTP password to delete from keyring: {}", e);
                    }

                    self.status_message = "Account removed".into();

                    // Clean up cached data for removed account
                    if let Some(cache) = &self.cache {
                        let cache = cache.clone();
                        return cosmic::task::future(async move {
                            if let Err(e) = cache.remove_account(removed_id).await {
                                log::warn!("Failed to clean cache for removed account: {}", e);
                            }
                            Message::Noop
                        });
                    }
                }
            }
            Message::ToggleAccountCollapse(idx) => {
                if let Some(acct) = self.accounts.get_mut(idx) {
                    acct.collapsed = !acct.collapsed;
                }
            }
            _ => {}
        }
        Task::none()
    }

    /// Save the current account list to the multi-account config file.
    pub(super) fn save_multi_account_config(&self) -> Result<(), String> {
        use neverlight_mail_core::config::{FileAccountConfig, MultiAccountFileConfig, PasswordBackend};

        let accounts: Vec<FileAccountConfig> = self
            .accounts
            .iter()
            .map(|a| FileAccountConfig {
                id: a.config.id.clone(),
                label: a.config.label.clone(),
                server: a.config.imap_server.clone(),
                port: a.config.imap_port,
                username: a.config.username.clone(),
                starttls: a.config.use_starttls,
                password: PasswordBackend::Keyring,
                email_addresses: a.config.email_addresses.clone(),
                smtp: a.config.smtp_overrides.clone(),
            })
            .collect();

        let config = MultiAccountFileConfig { accounts };
        config.save()
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
