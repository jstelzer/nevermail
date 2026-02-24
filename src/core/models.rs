use serde::{Deserialize, Serialize};

/// A mail folder (IMAP mailbox).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Folder {
    pub name: String,
    pub path: String,
    pub unread_count: u32,
    pub total_count: u32,
    pub mailbox_hash: u64,
}

/// Summary of a message for the list view (no body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSummary {
    pub uid: u64,
    pub subject: String,
    pub from: String,
    pub date: String,
    pub is_read: bool,
    pub is_starred: bool,
    pub has_attachments: bool,
    pub thread_id: Option<u64>,
    pub envelope_hash: u64,
    pub timestamp: i64,
    pub mailbox_hash: u64,
    pub message_id: String,
    pub in_reply_to: Option<String>,
    pub thread_depth: u32,
}

/// Decoded attachment data for display and saving.
#[derive(Debug, Clone)]
pub struct AttachmentData {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

impl AttachmentData {
    pub fn is_image(&self) -> bool {
        self.mime_type
            .to_ascii_lowercase()
            .starts_with("image/")
    }
}

