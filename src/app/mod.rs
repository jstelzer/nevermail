mod accounts;
mod actions;
mod body;
mod compose;
mod layout;
mod navigation;
mod search;
mod setup;
mod sync;
mod types;
mod watch;

pub use types::*;

use std::collections::{HashMap, HashSet};

use cosmic::app::{Core, Task};
use cosmic::iced::keyboard;
use cosmic::iced::{Event, Length, Subscription};
use cosmic::widget;
use cosmic::widget::{pane_grid, text_editor};
use cosmic::Element;

use neverlight_mail_core::config::{ConfigNeedsInput, LayoutConfig};
use neverlight_mail_core::imap::ImapSession;
use neverlight_mail_core::setup::SetupModel;
use neverlight_mail_core::store::CacheHandle;

use crate::dnd_models::DraggedFiles;
use crate::ui::compose_dialog::ComposeMode;

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
        let cache = match CacheHandle::open("cosmic") {
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
            body_defer_retries: 0,
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
}
