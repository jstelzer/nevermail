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
}

/// Full message body for the preview pane.
#[derive(Debug, Clone)]
pub struct MessageBody {
    pub uid: u64,
    pub text_plain: Option<String>,
    pub text_html: Option<String>,
    pub rendered: String,
    pub attachments: Vec<Attachment>,
}

/// An email attachment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

/// Account connection state.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Error(String),
}
