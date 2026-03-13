mod accounts;
mod actions;
mod backfill;
mod body;
mod compose;
mod layout;
mod navigation;
mod search;
mod setup;
mod sync;
mod sync_apply;
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

use neverlight_mail_core::config::{AccountConfig, ConfigNeedsInput, LayoutConfig};
use neverlight_mail_core::session::JmapSession;
use neverlight_mail_core::setup::SetupModel;
use neverlight_mail_core::store::CacheHandle;

use crate::dnd_models::DraggedFiles;
use crate::ui::compose_dialog::ComposeMode;

/// Connect to an account via JMAP session discovery.
fn connect_account(config: AccountConfig, account_id: String) -> Task<Message> {
    let label = config.label.clone();
    log::info!("[{}] Connecting via JMAP (account={})", label, account_id);
    cosmic::task::future(async move {
        let result = JmapSession::connect(&config)
            .await
            .map(|(_session, client)| client);
        if let Err(ref e) = result {
            log::error!("[{}] JMAP connect failed: {}", label, e);
        }
        Message::AccountConnected {
            account_id,
            result,
        }
    })
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
            selected_mailbox_id: None,
            selected_folder_evicted: false,
            messages: Vec::new(),
            selected_message: None,
            messages_offset: 0,
            has_more_messages: false,
            preview_body: String::new(),
            preview_markdown: Vec::new(),
            preview_attachments: Vec::new(),
            preview_image_handles: Vec::new(),
            conversation: Vec::new(),
            active_conversation_id: None,
            collapsed_threads: HashSet::new(),
            visible_indices: Vec::new(),
            thread_sizes: HashMap::new(),
            pending_move_restore: HashMap::new(),
            pending_flag_epochs: HashMap::new(),
            pending_move_epochs: HashMap::new(),
            search_abort: None,
            folder_abort: None,
            message_abort: None,
            body_abort: None,
            status_message: "Starting up...".into(),
            error_surface: None,
            phase: Phase::Loading,
            folder_epoch: 0,
            message_epoch: 0,
            search_epoch: 0,
            refresh_epoch: 0,
            mutation_epoch: 0,
            flag_epoch: 0,
            body_epoch: 0,
            refresh_phase: RefreshPhase::Idle,
            refresh_accounts_outstanding: HashSet::new(),
            refresh_started_at: None,
            mutation_in_flight_accounts: HashSet::new(),
            flag_in_flight_accounts: HashSet::new(),
            pending_move_intents: HashMap::new(),
            pending_flag_intents: HashMap::new(),
            notified_messages: HashSet::new(),
            stale_apply_drop_count: 0,
            toc_drift_count: 0,
            postcondition_failure_count: 0,
            refresh_timeout_count: 0,
            refresh_stuck_count: 0,
            reconnect_count: 0,
            last_sync_at: None,
            last_refresh_at: None,

            search_phase: SearchPhase::Inactive,
            search_query: String::new(),

            compose_phase: ComposePhase::Closed,
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
            compose_account_labels: Vec::new(),
            compose_cached_from: Vec::new(),

            setup_model: None,
            setup_password_visible: false,
            confirm_delete_account_id: None,
            oauth_phase: OAuthSetupPhase::Inactive,
            oauth_error: None,

            folder_drag_target: None,
            pending_body: None,
            body_defer_retries: 0,
            auto_read_suppressed: false,

            panes,
            diagnostics_collapsed: true,
        };

        let title_task = app.set_window_title("Nevermail".into());
        let mut tasks = vec![title_task];

        // Resolve config: env → file+keyring → show dialog
        match neverlight_mail_core::config::resolve_all_accounts() {
            Ok(account_configs) => {
                for ac in account_configs {
                    let account_id = ac.id.clone();
                    let mut acct = AccountState::new(ac.clone());
                    acct.conn_state = ConnectionState::Connecting;
                    app.accounts.push(acct);

                    // Load cached folders for this account
                    if let Some(cache) = cache.clone() {
                        let aid = account_id.clone();
                        tasks.push(cosmic::task::future(async move {
                            let result = cache.load_folders(aid.clone()).await;
                            Message::CachedFoldersLoaded {
                                account_id: aid,
                                result,
                            }
                        }));
                    }

                    // Start connecting
                    let aid = account_id.clone();
                    tasks.push(connect_account(ac, aid));
                }
                if app.accounts.is_empty() {
                    app.setup_model =
                        Some(SetupModel::from_config_needs(&ConfigNeedsInput::FullSetup));
                    app.status_message = "Setup required — enter your account details".into();
                }
            }
            Err(ref needs) => {
                let status = match needs {
                    ConfigNeedsInput::FullSetup => "Setup required — enter your account details",
                    ConfigNeedsInput::TokenOnly { .. } => "API token required",
                    ConfigNeedsInput::OAuthReauth { .. } => "Authorization expired — re-authorize to reconnect",
                };
                app.setup_model = Some(SetupModel::from_config_needs(needs));
                app.status_message = status.into();
            }
        }

        (app, cosmic::task::batch(tasks))
    }

    fn dialog(&self) -> Option<Element<'_, Self::Message>> {
        if let Some(account_id) = &self.confirm_delete_account_id {
            let label = self
                .accounts
                .iter()
                .find(|a| &a.config.id == account_id)
                .map(|a| a.config.label.clone())
                .unwrap_or_else(|| account_id.clone());
            let dialog = widget::dialog()
                .title("Delete Account")
                .body(format!(
                    "Delete account \"{}\"? This removes local cache and saved credentials.",
                    label
                ))
                .primary_action(
                    widget::button::destructive("Delete").on_press(Message::ConfirmDeleteAccount),
                )
                .secondary_action(
                    widget::button::standard("Cancel").on_press(Message::CancelDeleteAccount),
                );
            return Some(dialog.into());
        }
        if self.setup_model.is_some() {
            return Some(self.setup_dialog());
        }
        if self.compose_phase.is_open() {
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
                    is_sending: self.compose_phase == ComposePhase::Sending,
                    drag_hover: self.compose_drag_hover,
                },
            ));
        }
        None
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        let mut subs = Vec::new();

        if self.search_phase.is_focused() {
            // When search input has focus, only intercept Escape.
            subs.push(cosmic::iced_futures::event::listen_raw(
                |event, status, _| {
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
                },
            ));
        } else {
            // Full keyboard shortcuts when not typing in search
            subs.push(cosmic::iced_futures::event::listen_raw(
                |event, status, _| {
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
                            keyboard::Key::Named(keyboard::key::Named::F5) => {
                                Some(Message::Refresh)
                            }
                            _ => None,
                        },
                        _ => None,
                    }
                },
            ));
        }

        // Per-account EventSource push streams
        for (i, acct) in self.accounts.iter().enumerate() {
            if let Some(client) = &acct.client {
                let client = client.clone();
                let account_id = acct.config.id.clone();
                let sub_id = format!("push-watch-{}", i);
                subs.push(
                    Subscription::run_with_id(
                        sub_id,
                        watch::push_watch_stream(client, account_id.clone()),
                    )
                    .map(move |msg| msg),
                );
            }
        }

        // Per-account backfill streams
        for (i, acct) in self.accounts.iter().enumerate() {
            if acct.backfill_active {
                if let (Some(client), Some(cache)) = (&acct.client, &self.cache) {
                    let mailbox_ids: Vec<String> =
                        acct.folders.iter().map(|f| f.mailbox_id.clone()).collect();
                    if !mailbox_ids.is_empty() {
                        subs.push(Subscription::run_with_id(
                            format!("backfill-{}", i),
                            backfill::backfill_stream(
                                client.clone(),
                                cache.clone(),
                                acct.config.id.clone(),
                                mailbox_ids,
                                acct.config.max_messages_per_mailbox,
                                acct.backfill_pause.clone(),
                            ),
                        ));
                    }
                }
            }
        }

        // Periodic full sync fallback (5 minutes)
        let has_any_client = self.accounts.iter().any(|a| a.client.is_some());
        if has_any_client {
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
                    crate::ui::sidebar::DiagnosticsState {
                        collapsed: self.diagnostics_collapsed,
                        phase: self.phase,
                        selected_folder_evicted: self.selected_folder_evicted,
                        stale_apply_drop_count: self.stale_apply_drop_count,
                        toc_drift_count: self.toc_drift_count,
                        postcondition_failure_count: self.postcondition_failure_count,
                        refresh_timeout_count: self.refresh_timeout_count,
                        refresh_stuck_count: self.refresh_stuck_count,
                        reconnect_count: self.reconnect_count,
                        error_surface: self.error_surface.as_ref(),
                        accounts: &self.accounts,
                        last_sync_at: self.last_sync_at,
                        last_refresh_at: self.last_refresh_at,
                        refresh_in_flight: self.refresh_phase.is_in_flight(),
                    },
                ),
                PaneKind::MessageList => crate::ui::message_list::view(
                    crate::ui::message_list::MessageListState {
                        messages: &self.messages,
                        visible_indices: &self.visible_indices,
                        selected: self.selected_message,
                        has_more: self.has_more_messages && !self.search_phase.is_active(),
                        collapsed_threads: &self.collapsed_threads,
                        thread_sizes: &self.thread_sizes,
                        search_active: self.search_phase.is_active(),
                        search_query: &self.search_query,
                    },
                ),
                PaneKind::MessageView => {
                    let selected_msg = self
                        .selected_message
                        .and_then(|i| self.messages.get(i).map(|msg| (i, msg)));
                    crate::ui::message_view::view(
                        &self.preview_markdown,
                        selected_msg,
                        &self.preview_attachments,
                        &self.preview_image_handles,
                        &self.conversation,
                        self.active_conversation_id.as_deref(),
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
            | Message::SetupJmapUrlChanged(_)
            | Message::SetupUsernameChanged(_)
            | Message::SetupTokenChanged(_)
            | Message::SetupPasswordVisibilityToggled
            | Message::SetupEmailAddressesChanged(_)
            | Message::SetupSubmit
            | Message::SetupCancel
            | Message::SetupOAuthStart
            | Message::SetupOAuthTokensReceived(_) => self.handle_setup(message),

            // Account management
            Message::AccountAdd
            | Message::AccountEdit(_)
            | Message::RequestDeleteAccount(_)
            | Message::ConfirmDeleteAccount
            | Message::CancelDeleteAccount
            | Message::ToggleAccountCollapse(_) => self.handle_account_management(message),

            // Sync / connection / folder selection
            Message::AccountConnected { .. }
            | Message::CachedFoldersLoaded { .. }
            | Message::CachedMessagesLoaded { .. }
            | Message::SyncFoldersComplete { .. }
            | Message::SyncMessagesComplete { .. }
            | Message::SelectFolder(_, _)
            | Message::LoadMoreMessages
            | Message::ForceReconnect(_)
            | Message::Refresh => self.handle_sync(message),

            // Body / attachment viewing
            Message::ViewBody(_)
            | Message::BodyDeferred { .. }
            | Message::BodyLoaded { .. }
            | Message::ThreadLoaded { .. }
            | Message::ConversationBodyLoaded { .. }
            | Message::SetActiveConversation(_)
            | Message::SaveConversationAttachment { .. }
            | Message::LinkClicked(_)
            | Message::CopyBody
            | Message::SaveAttachment(_)
            | Message::SaveAttachmentComplete(_) => self.handle_body(message),

            // Flag / move actions
            Message::ToggleRead(_)
            | Message::ToggleStar(_)
            | Message::Delete(_)
            | Message::AutoMarkRead(_)
            | Message::Trash(_)
            | Message::Archive(_)
            | Message::RunFlagIntent(_)
            | Message::RunMoveIntent(_)
            | Message::DragMessageToFolder { .. }
            | Message::FolderDragEnter(_)
            | Message::FolderDragLeave
            | Message::FlagOpComplete { .. }
            | Message::MoveOpComplete { .. }
            => self.handle_actions(message),

            // Keyboard navigation
            Message::SelectionUp
            | Message::SelectionDown
            | Message::ActivateSelection
            | Message::ToggleThreadCollapse => self.handle_navigation(message),

            // Search
            Message::SearchActivate
            | Message::SearchQueryChanged(_)
            | Message::SearchExecute
            | Message::SearchResultsLoaded { .. }
            | Message::SearchClear => self.handle_search(message),

            // EventSource push events
            Message::PushStateChanged(_)
            | Message::PushError(_, _)
            | Message::PushEnded(_) => self.handle_watch(message),

            // Backfill progress
            Message::BackfillProgress { .. }
            | Message::BackfillComplete(_)
            | Message::BackfillTrigger { .. } => self.handle_backfill(message),

            // Pane layout
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => {
                self.panes.resize(split, ratio);
                self.save_layout();
                Task::none()
            }
            Message::ToggleDiagnostics => {
                self.diagnostics_collapsed = !self.diagnostics_collapsed;
                Task::none()
            }
            Message::Noop => Task::none(),
        }
    }
}

impl AppModel {
    pub(super) fn clear_error_surface(&mut self) {
        self.error_surface = None;
    }

    /// Transition refresh to idle and unpause backfill.
    pub(super) fn finish_refresh(&mut self) {
        self.refresh_phase = RefreshPhase::Idle;
        self.refresh_started_at = None;
        for acct in &self.accounts {
            acct.backfill_pause.store(false, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(super) fn set_status_error(&mut self, message: String) {
        self.error_surface = Some(ErrorSurface::Status {
            message: message.clone(),
        });
        self.status_message = message;
        self.phase = Phase::Error;
    }

    pub(super) fn set_recoverable_action_error(&mut self, error: RecoverableActionError) {
        self.status_message = error.message.clone();
        self.error_surface = Some(ErrorSurface::RecoverableAction(error));
    }

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
