use tokio::sync::oneshot;

use crate::core::models::{AttachmentData, Folder, MessageSummary};

#[allow(clippy::type_complexity)]
pub(super) enum CacheCmd {
    SaveFolders {
        account_id: String,
        folders: Vec<Folder>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    LoadFolders {
        account_id: String,
        reply: oneshot::Sender<Result<Vec<Folder>, String>>,
    },
    SaveMessages {
        account_id: String,
        mailbox_hash: u64,
        messages: Vec<MessageSummary>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    LoadMessages {
        account_id: String,
        mailbox_hash: u64,
        limit: u32,
        offset: u32,
        reply: oneshot::Sender<Result<Vec<MessageSummary>, String>>,
    },
    LoadBody {
        envelope_hash: u64,
        reply: oneshot::Sender<Result<Option<(String, String, Vec<AttachmentData>)>, String>>,
    },
    SaveBody {
        envelope_hash: u64,
        body_markdown: String,
        body_plain: String,
        attachments: Vec<AttachmentData>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    // Phase 2b: dual-truth flag ops
    UpdateFlags {
        envelope_hash: u64,
        flags_local: u8,
        pending_op: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ClearPendingOp {
        envelope_hash: u64,
        flags_server: u8,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RevertPendingOp {
        envelope_hash: u64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    RemoveMessage {
        envelope_hash: u64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Search {
        query: String,
        reply: oneshot::Sender<Result<Vec<MessageSummary>, String>>,
    },
    RemoveAccount {
        account_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
}
