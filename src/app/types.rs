use std::collections::{HashMap, HashSet};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use cosmic::app::Core;
use cosmic::widget::{image, markdown, pane_grid, text_editor};
use futures::future::AbortHandle;

use neverlight_mail_core::client::JmapClient;
use neverlight_mail_core::config::{AccountConfig, AccountId};
use neverlight_mail_core::models::{AttachmentData, Folder, MessageSummary};
use neverlight_mail_core::setup::SetupModel;
use neverlight_mail_core::store::CacheHandle;

use crate::dnd_models::DraggedFiles;
use crate::ui::compose_dialog::ComposeMode;

#[derive(Debug, Clone)]
pub struct ConversationEntry {
    pub email_id: String,
    pub summary: MessageSummary,
    pub markdown_items: Vec<markdown::Item>,
    pub plain_body: String,
    pub attachments: Vec<AttachmentData>,
    pub image_handles: Vec<Option<image::Handle>>,
    pub is_sent: bool,
    pub loaded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    Sidebar,
    MessageList,
    MessageView,
}

pub(crate) const APP_ID: &str = "com.neverlight.email";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Syncing,
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Loading,
    Refreshing,
    Searching,
    Error,
}

/// Compose dialog lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposePhase {
    Closed,
    Open,
    Sending,
}

impl ComposePhase {
    pub fn is_open(self) -> bool {
        !matches!(self, Self::Closed)
    }
}

/// Search bar lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPhase {
    /// No search UI visible.
    Inactive,
    /// Search input visible, keyboard routes to input.
    InputFocused,
    /// Search results displayed, keyboard back to normal shortcuts.
    Results,
}

impl SearchPhase {
    pub fn is_active(self) -> bool {
        !matches!(self, Self::Inactive)
    }

    pub fn is_focused(self) -> bool {
        matches!(self, Self::InputFocused)
    }
}

/// Refresh lane coalescing state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshPhase {
    Idle,
    InFlight { pending: bool, timeout_reported: bool },
}

impl RefreshPhase {
    pub fn is_in_flight(self) -> bool {
        matches!(self, Self::InFlight { .. })
    }

    pub fn set_pending(&mut self) {
        if let Self::InFlight { pending, .. } = self {
            *pending = true;
        }
    }

    pub fn take_pending(&mut self) -> bool {
        if let Self::InFlight { pending, .. } = self {
            let was = *pending;
            *pending = false;
            was
        } else {
            false
        }
    }

    pub fn mark_timeout_reported(&mut self) {
        if let Self::InFlight { timeout_reported, .. } = self {
            *timeout_reported = true;
        }
    }

    pub fn is_timeout_reported(self) -> bool {
        matches!(self, Self::InFlight { timeout_reported: true, .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Move,
    Flag,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAction {
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoverableActionError {
    pub action: ActionKind,
    pub message: String,
    pub retry: RetryAction,
    pub email_id: Option<String>,
    pub mailbox_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorSurface {
    RecoverableAction(RecoverableActionError),
    Status { message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagIntentKind {
    ToggleRead,
    ToggleStar,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MailboxIdentity {
    pub account_id: AccountId,
    pub mailbox_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MessageIdentity {
    pub account_id: AccountId,
    pub mailbox_id: String,
    pub email_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFlagIntent {
    pub message: MessageIdentity,
    pub kind: FlagIntentKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingMoveIntent {
    pub message: MessageIdentity,
    pub source: MailboxIdentity,
    pub dest: MailboxIdentity,
}

// ---------------------------------------------------------------------------
// Per-account state
// ---------------------------------------------------------------------------

pub struct AccountState {
    pub config: AccountConfig,
    pub client: Option<JmapClient>,
    pub conn_state: ConnectionState,
    pub folders: Vec<Folder>,
    /// Maps mailbox path → JMAP mailbox ID.
    pub folder_map: HashMap<String, String>,
    pub collapsed: bool,
    /// Consecutive reconnect failures (reset on success).
    pub reconnect_attempts: u32,
    /// Last error message for diagnostics display.
    pub last_error: Option<String>,
    /// Per-mailbox backfill progress: mailbox_id → (position, total).
    pub backfill_progress: HashMap<String, (u32, u32)>,
    /// Whether backfill subscription is active for this account.
    pub backfill_active: bool,
    /// Pause flag: set during head sync to avoid contention.
    pub backfill_pause: Arc<AtomicBool>,
}

impl AccountState {
    pub fn new(config: AccountConfig) -> Self {
        AccountState {
            config,
            client: None,
            conn_state: ConnectionState::Disconnected,
            folders: Vec::new(),
            folder_map: HashMap::new(),
            collapsed: false,
            reconnect_attempts: 0,
            last_error: None,
            backfill_progress: HashMap::new(),
            backfill_active: false,
            backfill_pause: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Backoff duration for reconnect retries: 5s, 15s, 30s, 60s cap.
    pub fn reconnect_backoff(&self) -> std::time::Duration {
        let secs = match self.reconnect_attempts {
            0 => 5,
            1 => 15,
            2 => 30,
            _ => 60,
        };
        std::time::Duration::from_secs(secs)
    }

    pub fn rebuild_folder_map(&mut self) {
        self.folder_map.clear();
        for f in &self.folders {
            self.folder_map
                .insert(f.path.clone(), f.mailbox_id.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// AppModel
// ---------------------------------------------------------------------------

pub struct AppModel {
    pub(crate) core: Core,

    // Multi-account state
    pub(super) accounts: Vec<AccountState>,
    pub(super) active_account: Option<usize>,

    pub(super) cache: Option<CacheHandle>,

    pub(super) selected_folder: Option<usize>,
    pub(super) selected_mailbox_id: Option<String>,
    pub(super) selected_folder_evicted: bool,

    pub(super) messages: Vec<MessageSummary>,
    pub(super) selected_message: Option<usize>,
    pub(super) messages_offset: u32,
    pub(super) has_more_messages: bool,

    pub(super) preview_body: String,
    pub(super) preview_markdown: Vec<markdown::Item>,
    pub(super) preview_attachments: Vec<AttachmentData>,
    pub(super) preview_image_handles: Vec<Option<image::Handle>>,

    /// Conversation view: all messages in the selected thread, bodies loading progressively.
    pub(super) conversation: Vec<ConversationEntry>,
    /// The email_id of the "active" message within the conversation (reply/forward target).
    pub(super) active_conversation_id: Option<String>,

    /// Thread IDs that are currently collapsed (children hidden)
    pub(super) collapsed_threads: HashSet<String>,
    /// Maps visible row positions → real indices into `messages`
    pub(super) visible_indices: Vec<usize>,
    /// Total messages per thread_id (for collapse indicators)
    pub(super) thread_sizes: HashMap<String, usize>,
    /// Snapshot of optimistically removed messages for move rollback.
    pub(super) pending_move_restore: HashMap<MessageIdentity, (MessageSummary, usize)>,
    /// Latest flag operation epoch per envelope (stale completions are dropped).
    pub(super) pending_flag_epochs: HashMap<MessageIdentity, u64>,
    /// Latest move operation epoch per envelope (stale completions are dropped).
    pub(super) pending_move_epochs: HashMap<MessageIdentity, u64>,
    /// Abort handles for true in-flight cancellation of superseded lane operations.
    pub(super) search_abort: Option<AbortHandle>,
    pub(super) folder_abort: Option<AbortHandle>,
    pub(super) message_abort: Option<AbortHandle>,
    pub(super) body_abort: Option<AbortHandle>,

    pub(super) status_message: String,
    pub(super) error_surface: Option<ErrorSurface>,
    pub(super) phase: Phase,
    /// Monotonic epochs by lane.
    pub(super) folder_epoch: u64,
    pub(super) message_epoch: u64,
    pub(super) search_epoch: u64,
    pub(super) refresh_epoch: u64,
    pub(super) mutation_epoch: u64,
    pub(super) flag_epoch: u64,
    pub(super) body_epoch: u64,
    /// Refresh lane coalescing state.
    pub(super) refresh_phase: RefreshPhase,
    pub(super) refresh_accounts_outstanding: HashSet<AccountId>,
    pub(super) refresh_started_at: Option<Instant>,
    pub(super) mutation_in_flight_accounts: HashSet<AccountId>,
    pub(super) flag_in_flight_accounts: HashSet<AccountId>,
    pub(super) pending_move_intents: HashMap<AccountId, PendingMoveIntent>,
    pub(super) pending_flag_intents: HashMap<AccountId, PendingFlagIntent>,
    /// Recently notified messages (dedup push events).
    pub(super) notified_messages: HashSet<MessageIdentity>,
    /// Diagnostics counters.
    pub(super) stale_apply_drop_count: u64,
    pub(super) toc_drift_count: u64,
    pub(super) postcondition_failure_count: u64,
    pub(super) refresh_timeout_count: u64,
    pub(super) refresh_stuck_count: u64,
    pub(super) reconnect_count: u64,
    /// Timing for diagnostics.
    pub(super) last_sync_at: Option<Instant>,
    pub(super) last_refresh_at: Option<Instant>,

    // Search state
    pub(super) search_phase: SearchPhase,
    pub(super) search_query: String,

    // Compose dialog state
    pub(super) compose_phase: ComposePhase,
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
    // Cached for dialog() lifetime (updated when compose_account changes)
    pub(super) compose_account_labels: Vec<String>,
    pub(super) compose_cached_from: Vec<String>,

    // Setup dialog state — core fields live in SetupModel, visibility is local
    pub(super) setup_model: Option<SetupModel>,
    pub(super) setup_password_visible: bool,
    pub(super) confirm_delete_account_id: Option<AccountId>,
    pub(super) oauth_phase: OAuthSetupPhase,
    pub(super) oauth_error: Option<String>,

    // DnD state
    pub(super) folder_drag_target: Option<usize>,

    /// Body view deferred until connection is ready
    pub(super) pending_body: Option<usize>,
    /// Retry count for deferred body fetches (prevents infinite loops)
    pub(super) body_defer_retries: u8,

    /// Auto-mark-read: suppressed when user manually toggles back to unread
    pub(super) auto_read_suppressed: bool,

    // Pane layout
    pub(super) panes: pane_grid::State<PaneKind>,
    pub(super) diagnostics_collapsed: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    AccountConnected {
        account_id: AccountId,
        result: Result<JmapClient, String>,
    },

    SelectFolder(usize, usize), // (account_idx, folder_idx)

    ViewBody(usize),
    BodyDeferred {
        email_id: String,
        epoch: u64,
    },
    BodyLoaded {
        email_id: String,
        epoch: u64,
        result: Result<(String, String, Vec<AttachmentData>), String>,
    },
    LinkClicked(markdown::Url),
    CopyBody,

    SaveAttachment(usize),
    SaveAttachmentComplete(Result<String, String>),

    ThreadLoaded {
        thread_id: String,
        email_id: String,
        epoch: u64,
        result: Result<Vec<MessageSummary>, String>,
    },
    ConversationBodyLoaded {
        thread_id: String,
        email_id: String,
        epoch: u64,
        result: Result<(String, String, Vec<AttachmentData>), String>,
    },
    SetActiveConversation(String),
    SaveConversationAttachment { email_id: String, index: usize },

    // Cache-first messages
    CachedFoldersLoaded {
        account_id: AccountId,
        result: Result<Vec<Folder>, String>,
    },
    CachedMessagesLoaded {
        account_id: AccountId,
        mailbox_id: String,
        offset: u32,
        epoch: u64,
        result: Result<Vec<MessageSummary>, String>,
    },
    SyncFoldersComplete {
        account_id: AccountId,
        epoch: u64,
        result: Result<Vec<Folder>, String>,
    },
    SyncMessagesComplete {
        account_id: AccountId,
        mailbox_id: String,
        epoch: u64,
        result: Result<(), String>,
    },
    LoadMoreMessages,

    // Flag/move actions
    ToggleRead(usize),
    ToggleStar(usize),
    Delete(usize),
    Trash(usize),
    Archive(usize),
    RunFlagIntent(PendingFlagIntent),
    RunMoveIntent(PendingMoveIntent),
    FlagOpComplete {
        message: MessageIdentity,
        epoch: u64,
        prev_flags: u8,
        result: Result<u8, String>,
    },
    MoveOpComplete {
        message: MessageIdentity,
        source: MailboxIdentity,
        epoch: u64,
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

    /// EventSource push: server state changed, trigger delta sync.
    PushStateChanged(AccountId),
    /// EventSource stream ended or errored — schedule reconnect.
    PushError(AccountId, String),
    PushEnded(AccountId),

    // Search
    SearchActivate,
    SearchQueryChanged(String),
    SearchExecute,
    SearchResultsLoaded {
        query: String,
        epoch: u64,
        result: Result<Vec<MessageSummary>, String>,
    },
    SearchClear,

    // Message-to-folder drag
    DragMessageToFolder {
        message: MessageIdentity,
        source: MailboxIdentity,
        dest: MailboxIdentity,
    },
    FolderDragEnter(usize),
    FolderDragLeave,

    PaneResized(pane_grid::ResizeEvent),
    ToggleDiagnostics,

    /// Auto-mark-read: fires 5s after a message is displayed
    AutoMarkRead(String),

    // Backfill progress from background history walking
    BackfillProgress {
        account_id: AccountId,
        mailbox_id: String,
        position: u32,
        total: u32,
        completed: bool,
    },
    BackfillComplete(AccountId),
    BackfillTrigger {
        account_id: AccountId,
        mailbox_id: String,
    },

    ForceReconnect(AccountId),
    Refresh,
    Noop,

    // Account management
    AccountAdd,
    AccountEdit(AccountId),
    RequestDeleteAccount(AccountId),
    ConfirmDeleteAccount,
    CancelDeleteAccount,
    ToggleAccountCollapse(usize),

    // Setup dialog messages (JMAP-only: 5 fields)
    SetupLabelChanged(String),
    SetupJmapUrlChanged(String),
    SetupUsernameChanged(String),
    SetupTokenChanged(String),
    SetupEmailAddressesChanged(String),
    SetupPasswordVisibilityToggled,
    SetupSubmit,
    SetupCancel,

    // OAuth flow messages
    SetupOAuthStart,
    SetupOAuthTokensReceived(Result<OAuthTokenResult, String>),
}

/// OAuth tokens received from the full browser-based flow.
#[derive(Debug, Clone)]
pub struct OAuthTokenResult {
    pub issuer: String,
    pub client_id: String,
    pub token_endpoint: String,
    pub resource: String,
    pub access_token: String,
    pub refresh_token: String,
}

/// OAuth setup progress phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthSetupPhase {
    /// Not in OAuth flow.
    Inactive,
    /// Discovering OAuth metadata + registering client + waiting for browser.
    Discovering,
}
