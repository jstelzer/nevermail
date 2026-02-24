use std::path::PathBuf;

use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::core::models::{AttachmentData, Folder, MessageSummary};

const PAGE_SIZE: u32 = 50;

/// Schema DDL run on open.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS folders (
    path TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    mailbox_hash INTEGER NOT NULL UNIQUE,
    unread_count INTEGER DEFAULT 0,
    total_count INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS messages (
    envelope_hash INTEGER PRIMARY KEY,
    mailbox_hash INTEGER NOT NULL,
    subject TEXT,
    sender TEXT,
    date TEXT,
    timestamp INTEGER NOT NULL DEFAULT 0,
    is_read INTEGER DEFAULT 0,
    is_starred INTEGER DEFAULT 0,
    has_attachments INTEGER DEFAULT 0,
    thread_id INTEGER,
    body_rendered TEXT,
    FOREIGN KEY (mailbox_hash) REFERENCES folders(mailbox_hash)
);

CREATE INDEX IF NOT EXISTS idx_messages_mailbox
    ON messages(mailbox_hash, timestamp DESC);

CREATE TABLE IF NOT EXISTS attachments (
    envelope_hash INTEGER NOT NULL,
    idx INTEGER NOT NULL,
    filename TEXT NOT NULL DEFAULT 'unnamed',
    mime_type TEXT NOT NULL DEFAULT 'application/octet-stream',
    data BLOB NOT NULL,
    PRIMARY KEY (envelope_hash, idx)
);
";

/// Run forward-only migrations. Each ALTER is idempotent (ignores "duplicate column" errors).
fn run_migrations(conn: &Connection) {
    let alters = [
        "ALTER TABLE messages ADD COLUMN flags_server INTEGER DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN flags_local INTEGER DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN pending_op TEXT",
        "ALTER TABLE messages ADD COLUMN message_id TEXT",
        "ALTER TABLE messages ADD COLUMN in_reply_to TEXT",
        "ALTER TABLE messages ADD COLUMN thread_depth INTEGER DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN body_markdown TEXT",
    ];
    for sql in &alters {
        // "duplicate column name" is the expected error when already migrated
        if let Err(e) = conn.execute(sql, []) {
            let msg = e.to_string();
            if !msg.contains("duplicate column") {
                log::warn!("Migration failed ({}): {}", sql, msg);
            }
        }
    }

    // Indexes (idempotent via IF NOT EXISTS)
    if let Err(e) = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_messages_message_id ON messages(message_id)",
        [],
    ) {
        log::warn!("Index creation failed: {}", e);
    }

    // FTS5 full-text search index (external content, keyed to messages rowid).
    // Column names MUST match the content table for rebuild to work.
    // Drop stale FTS objects from earlier schema that used wrong column name ('body').
    for stale in &[
        "DROP TRIGGER IF EXISTS messages_fts_ai",
        "DROP TRIGGER IF EXISTS messages_fts_ad",
        "DROP TRIGGER IF EXISTS messages_fts_au",
        "DROP TABLE IF EXISTS message_fts",
    ] {
        let _ = conn.execute_batch(stale);
    }

    let fts_ddl = [
        "CREATE VIRTUAL TABLE IF NOT EXISTS message_fts USING fts5(
            subject,
            sender,
            body_rendered,
            content='messages',
            content_rowid='rowid'
        )",
        // Auto-sync triggers
        "CREATE TRIGGER IF NOT EXISTS messages_fts_ai AFTER INSERT ON messages BEGIN
          INSERT INTO message_fts(rowid, subject, sender, body_rendered)
          VALUES (new.rowid, new.subject, new.sender, new.body_rendered);
        END",
        "CREATE TRIGGER IF NOT EXISTS messages_fts_ad AFTER DELETE ON messages BEGIN
          INSERT INTO message_fts(message_fts, rowid, subject, sender, body_rendered)
          VALUES('delete', old.rowid, old.subject, old.sender, old.body_rendered);
        END",
        "CREATE TRIGGER IF NOT EXISTS messages_fts_au AFTER UPDATE ON messages BEGIN
          INSERT INTO message_fts(message_fts, rowid, subject, sender, body_rendered)
          VALUES('delete', old.rowid, old.subject, old.sender, old.body_rendered);
          INSERT INTO message_fts(rowid, subject, sender, body_rendered)
          VALUES (new.rowid, new.subject, new.sender, new.body_rendered);
        END",
    ];
    for ddl in &fts_ddl {
        if let Err(e) = conn.execute_batch(ddl) {
            log::warn!("FTS5 migration failed ({}): {}", ddl.chars().take(60).collect::<String>(), e);
        }
    }

    // Rebuild FTS index from existing content (idempotent, fast if current)
    if let Err(e) = conn.execute("INSERT INTO message_fts(message_fts) VALUES('rebuild')", []) {
        log::warn!("FTS5 rebuild failed: {}", e);
    }
}

// ---------------------------------------------------------------------------
// Flag helpers — encode melib::Flag bitfield as u8
// ---------------------------------------------------------------------------

/// melib::Flag::SEEN  = 0b0000_0001
/// melib::Flag::FLAGGED = 0b0100_0000  (but we store our own compact encoding)
/// We use a simple two-bit encoding for the flags we care about:
///   bit 0 = SEEN
///   bit 1 = FLAGGED
pub fn flags_to_u8(is_read: bool, is_starred: bool) -> u8 {
    let mut f: u8 = 0;
    if is_read {
        f |= 1;
    }
    if is_starred {
        f |= 2;
    }
    f
}

pub fn flags_from_u8(f: u8) -> (bool, bool) {
    (f & 1 != 0, f & 2 != 0)
}

// ---------------------------------------------------------------------------
// Commands sent from async world → background thread
// ---------------------------------------------------------------------------

enum CacheCmd {
    SaveFolders {
        folders: Vec<Folder>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    LoadFolders {
        reply: oneshot::Sender<Result<Vec<Folder>, String>>,
    },
    SaveMessages {
        mailbox_hash: u64,
        messages: Vec<MessageSummary>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    LoadMessages {
        mailbox_hash: u64,
        limit: u32,
        offset: u32,
        reply: oneshot::Sender<Result<Vec<MessageSummary>, String>>,
    },
    MessageCount {
        mailbox_hash: u64,
        reply: oneshot::Sender<Result<u32, String>>,
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
}

// ---------------------------------------------------------------------------
// CacheHandle — Clone + Send + Sync async facade
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct CacheHandle {
    tx: mpsc::UnboundedSender<CacheCmd>,
}

impl CacheHandle {
    /// Open (or create) the cache database and spawn the background thread.
    pub fn open() -> Result<Self, String> {
        let db_path = Self::resolve_path()?;

        std::fs::create_dir_all(&db_path).map_err(|e| format!("Failed to create cache dir: {e}"))?;

        let db_file = db_path.join("cache.db");
        let conn =
            Connection::open(&db_file).map_err(|e| format!("Failed to open cache db: {e}"))?;

        conn.execute_batch(SCHEMA)
            .map_err(|e| format!("Failed to init cache schema: {e}"))?;

        run_migrations(&conn);

        let (tx, rx) = mpsc::unbounded_channel();

        std::thread::Builder::new()
            .name("nevermail-cache".into())
            .spawn(move || Self::run_loop(conn, rx))
            .map_err(|e| format!("Failed to spawn cache thread: {e}"))?;

        Ok(CacheHandle { tx })
    }

    fn resolve_path() -> Result<PathBuf, String> {
        let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
        Ok(base.join("nevermail"))
    }

    // -- async methods -------------------------------------------------------

    pub async fn save_folders(&self, folders: Vec<Folder>) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::SaveFolders { folders, reply })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    pub async fn load_folders(&self) -> Result<Vec<Folder>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::LoadFolders { reply })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    pub async fn save_messages(
        &self,
        mailbox_hash: u64,
        messages: Vec<MessageSummary>,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::SaveMessages {
                mailbox_hash,
                messages,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    pub async fn load_messages(
        &self,
        mailbox_hash: u64,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<MessageSummary>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::LoadMessages {
                mailbox_hash,
                limit,
                offset,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    pub async fn message_count(&self, mailbox_hash: u64) -> Result<u32, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::MessageCount {
                mailbox_hash,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    pub async fn load_body(
        &self,
        envelope_hash: u64,
    ) -> Result<Option<(String, String, Vec<AttachmentData>)>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::LoadBody {
                envelope_hash,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    pub async fn save_body(
        &self,
        envelope_hash: u64,
        body_markdown: String,
        body_plain: String,
        attachments: Vec<AttachmentData>,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::SaveBody {
                envelope_hash,
                body_markdown,
                body_plain,
                attachments,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    /// Set local flags and mark a pending operation.
    pub async fn update_flags(
        &self,
        envelope_hash: u64,
        flags_local: u8,
        pending_op: String,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::UpdateFlags {
                envelope_hash,
                flags_local,
                pending_op,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    /// IMAP op succeeded — update server flags and clear pending.
    pub async fn clear_pending_op(
        &self,
        envelope_hash: u64,
        flags_server: u8,
    ) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::ClearPendingOp {
                envelope_hash,
                flags_server,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    /// IMAP op failed — revert local flags to server flags, clear pending.
    pub async fn revert_pending_op(&self, envelope_hash: u64) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::RevertPendingOp {
                envelope_hash,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    /// Remove a message from the cache (after successful move).
    pub async fn remove_message(&self, envelope_hash: u64) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::RemoveMessage {
                envelope_hash,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    /// Full-text search across all folders.
    pub async fn search(&self, query: String) -> Result<Vec<MessageSummary>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::Search { query, reply })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    // -- background thread ---------------------------------------------------

    fn run_loop(conn: Connection, mut rx: mpsc::UnboundedReceiver<CacheCmd>) {
        while let Some(cmd) = rx.blocking_recv() {
            match cmd {
                CacheCmd::SaveFolders { folders, reply } => {
                    let _ = reply.send(Self::do_save_folders(&conn, &folders));
                }
                CacheCmd::LoadFolders { reply } => {
                    let _ = reply.send(Self::do_load_folders(&conn));
                }
                CacheCmd::SaveMessages {
                    mailbox_hash,
                    messages,
                    reply,
                } => {
                    let _ = reply.send(Self::do_save_messages(&conn, mailbox_hash, &messages));
                }
                CacheCmd::LoadMessages {
                    mailbox_hash,
                    limit,
                    offset,
                    reply,
                } => {
                    let _ =
                        reply.send(Self::do_load_messages(&conn, mailbox_hash, limit, offset));
                }
                CacheCmd::MessageCount {
                    mailbox_hash,
                    reply,
                } => {
                    let _ = reply.send(Self::do_message_count(&conn, mailbox_hash));
                }
                CacheCmd::LoadBody {
                    envelope_hash,
                    reply,
                } => {
                    let _ = reply.send(Self::do_load_body(&conn, envelope_hash));
                }
                CacheCmd::SaveBody {
                    envelope_hash,
                    body_markdown,
                    body_plain,
                    attachments,
                    reply,
                } => {
                    let _ = reply.send(Self::do_save_body(
                        &conn,
                        envelope_hash,
                        &body_markdown,
                        &body_plain,
                        &attachments,
                    ));
                }
                CacheCmd::UpdateFlags {
                    envelope_hash,
                    flags_local,
                    pending_op,
                    reply,
                } => {
                    let _ = reply.send(Self::do_update_flags(
                        &conn,
                        envelope_hash,
                        flags_local,
                        &pending_op,
                    ));
                }
                CacheCmd::ClearPendingOp {
                    envelope_hash,
                    flags_server,
                    reply,
                } => {
                    let _ =
                        reply.send(Self::do_clear_pending_op(&conn, envelope_hash, flags_server));
                }
                CacheCmd::RevertPendingOp {
                    envelope_hash,
                    reply,
                } => {
                    let _ = reply.send(Self::do_revert_pending_op(&conn, envelope_hash));
                }
                CacheCmd::RemoveMessage {
                    envelope_hash,
                    reply,
                } => {
                    let _ = reply.send(Self::do_remove_message(&conn, envelope_hash));
                }
                CacheCmd::Search { query, reply } => {
                    let _ = reply.send(Self::do_search(&conn, &query));
                }
            }
        }
        log::debug!("Cache thread exiting");
    }

    // -- synchronous DB operations -------------------------------------------

    fn do_save_folders(conn: &Connection, folders: &[Folder]) -> Result<(), String> {
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Cache tx error: {e}"))?;

        tx.execute("DELETE FROM folders", [])
            .map_err(|e| format!("Cache delete error: {e}"))?;

        let mut stmt = tx
            .prepare(
                "INSERT INTO folders (path, name, mailbox_hash, unread_count, total_count)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| format!("Cache prepare error: {e}"))?;

        for f in folders {
            stmt.execute(rusqlite::params![
                f.path,
                f.name,
                f.mailbox_hash as i64,
                f.unread_count,
                f.total_count,
            ])
            .map_err(|e| format!("Cache insert error: {e}"))?;
        }
        drop(stmt);

        tx.commit()
            .map_err(|e| format!("Cache commit error: {e}"))?;
        Ok(())
    }

    fn do_load_folders(conn: &Connection) -> Result<Vec<Folder>, String> {
        let mut stmt = conn
            .prepare("SELECT path, name, mailbox_hash, unread_count, total_count FROM folders")
            .map_err(|e| format!("Cache prepare error: {e}"))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(Folder {
                    path: row.get(0)?,
                    name: row.get(1)?,
                    mailbox_hash: row.get::<_, i64>(2)? as u64,
                    unread_count: row.get(3)?,
                    total_count: row.get(4)?,
                })
            })
            .map_err(|e| format!("Cache query error: {e}"))?;

        let mut folders = Vec::new();
        for row in rows {
            folders.push(row.map_err(|e| format!("Cache row error: {e}"))?);
        }

        // Sort: INBOX first, then alphabetical
        folders.sort_by(|a, b| {
            if a.path == "INBOX" {
                std::cmp::Ordering::Less
            } else if b.path == "INBOX" {
                std::cmp::Ordering::Greater
            } else {
                a.path.cmp(&b.path)
            }
        });

        Ok(folders)
    }

    fn do_save_messages(
        conn: &Connection,
        mailbox_hash: u64,
        messages: &[MessageSummary],
    ) -> Result<(), String> {
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Cache tx error: {e}"))?;

        // Collect envelope hashes that have pending ops — we must not overwrite those
        let mut pending_set = std::collections::HashSet::new();
        {
            let mut stmt = tx
                .prepare(
                    "SELECT envelope_hash FROM messages
                     WHERE mailbox_hash = ?1 AND pending_op IS NOT NULL",
                )
                .map_err(|e| format!("Cache prepare error: {e}"))?;
            let rows = stmt
                .query_map([mailbox_hash as i64], |row| row.get::<_, i64>(0))
                .map_err(|e| format!("Cache query error: {e}"))?;
            for row in rows {
                if let Ok(hash) = row {
                    pending_set.insert(hash as u64);
                }
            }
        }

        // Cascade: delete attachments for non-pending messages before removing message rows
        tx.execute(
            "DELETE FROM attachments WHERE envelope_hash IN (
                SELECT envelope_hash FROM messages WHERE mailbox_hash = ?1 AND pending_op IS NULL
            )",
            [mailbox_hash as i64],
        )
        .map_err(|e| format!("Cache attachment cascade error: {e}"))?;

        // Delete non-pending messages for this mailbox
        tx.execute(
            "DELETE FROM messages WHERE mailbox_hash = ?1 AND pending_op IS NULL",
            [mailbox_hash as i64],
        )
        .map_err(|e| format!("Cache delete error: {e}"))?;

        let mut stmt = tx
            .prepare(
                "INSERT OR IGNORE INTO messages
                 (envelope_hash, mailbox_hash, subject, sender, date, timestamp,
                  is_read, is_starred, has_attachments, thread_id, flags_server, flags_local,
                  message_id, in_reply_to, thread_depth)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            )
            .map_err(|e| format!("Cache prepare error: {e}"))?;

        // For messages with pending ops, update only flags_server (not flags_local or pending_op)
        let mut update_server_stmt = tx
            .prepare(
                "UPDATE messages SET flags_server = ?1, subject = ?2, sender = ?3,
                 date = ?4, timestamp = ?5, has_attachments = ?6, thread_id = ?7,
                 message_id = ?8, in_reply_to = ?9, thread_depth = ?10
                 WHERE envelope_hash = ?11 AND pending_op IS NOT NULL",
            )
            .map_err(|e| format!("Cache prepare error: {e}"))?;

        for m in messages {
            let server_flags = flags_to_u8(m.is_read, m.is_starred);

            if pending_set.contains(&m.envelope_hash) {
                // Update server-side data but preserve local overrides
                update_server_stmt
                    .execute(rusqlite::params![
                        server_flags as i32,
                        m.subject,
                        m.from,
                        m.date,
                        m.timestamp,
                        m.has_attachments as i32,
                        m.thread_id.map(|t| t as i64),
                        m.message_id,
                        m.in_reply_to,
                        m.thread_depth,
                        m.envelope_hash as i64,
                    ])
                    .map_err(|e| format!("Cache update error: {e}"))?;
            } else {
                // Fresh insert — server and local flags agree
                stmt.execute(rusqlite::params![
                    m.envelope_hash as i64,
                    mailbox_hash as i64,
                    m.subject,
                    m.from,
                    m.date,
                    m.timestamp,
                    m.is_read as i32,
                    m.is_starred as i32,
                    m.has_attachments as i32,
                    m.thread_id.map(|t| t as i64),
                    server_flags as i32,
                    server_flags as i32, // local = server when no pending op
                    m.message_id,
                    m.in_reply_to,
                    m.thread_depth,
                ])
                .map_err(|e| format!("Cache insert error: {e}"))?;
            }
        }
        drop(stmt);
        drop(update_server_stmt);

        tx.commit()
            .map_err(|e| format!("Cache commit error: {e}"))?;
        Ok(())
    }

    fn do_load_messages(
        conn: &Connection,
        mailbox_hash: u64,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<MessageSummary>, String> {
        let mut stmt = conn
            .prepare(
                "SELECT envelope_hash, subject, sender, date, timestamp,
                        is_read, is_starred, has_attachments, thread_id,
                        flags_server, flags_local, pending_op, mailbox_hash,
                        message_id, in_reply_to, thread_depth
                 FROM messages
                 WHERE mailbox_hash = ?1
                 ORDER BY
                     MAX(timestamp) OVER (
                         PARTITION BY COALESCE(thread_id, envelope_hash)
                     ) DESC,
                     COALESCE(thread_id, envelope_hash),
                     timestamp ASC
                 LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| format!("Cache prepare error: {e}"))?;

        let rows = stmt
            .query_map(
                rusqlite::params![mailbox_hash as i64, limit, offset],
                |row| {
                    let envelope_hash: i64 = row.get(0)?;
                    let thread_id: Option<i64> = row.get(8)?;
                    let flags_server: i32 = row.get::<_, Option<i32>>(9)?.unwrap_or(0);
                    let flags_local: i32 = row.get::<_, Option<i32>>(10)?.unwrap_or(0);
                    let pending_op: Option<String> = row.get(11)?;
                    let mbox_hash: i64 = row.get(12)?;

                    // Dual-truth: if pending_op is set, use flags_local; otherwise flags_server
                    let effective_flags = if pending_op.is_some() {
                        flags_local as u8
                    } else {
                        flags_server as u8
                    };
                    let (is_read, is_starred) = flags_from_u8(effective_flags);

                    Ok(MessageSummary {
                        uid: envelope_hash as u64,
                        subject: row.get(1)?,
                        from: row.get(2)?,
                        date: row.get(3)?,
                        timestamp: row.get(4)?,
                        is_read,
                        is_starred,
                        has_attachments: row.get::<_, i32>(7)? != 0,
                        thread_id: thread_id.map(|t| t as u64),
                        envelope_hash: envelope_hash as u64,
                        mailbox_hash: mbox_hash as u64,
                        message_id: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
                        in_reply_to: row.get(14)?,
                        thread_depth: row.get::<_, Option<u32>>(15)?.unwrap_or(0),
                    })
                },
            )
            .map_err(|e| format!("Cache query error: {e}"))?;

        let mut messages = Vec::new();
        for row in rows {
            messages.push(row.map_err(|e| format!("Cache row error: {e}"))?);
        }
        Ok(messages)
    }

    fn do_message_count(conn: &Connection, mailbox_hash: u64) -> Result<u32, String> {
        conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE mailbox_hash = ?1",
            [mailbox_hash as i64],
            |row| row.get(0),
        )
        .map_err(|e| format!("Cache count error: {e}"))
    }

    fn do_load_body(
        conn: &Connection,
        envelope_hash: u64,
    ) -> Result<Option<(String, String, Vec<AttachmentData>)>, String> {
        let row_result = conn.query_row(
            "SELECT body_rendered, body_markdown FROM messages WHERE envelope_hash = ?1",
            [envelope_hash as i64],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        );

        let (body_plain, body_markdown) = match row_result {
            Ok((Some(plain), md)) => (plain, md.unwrap_or_default()),
            Ok((None, _)) => return Ok(None),
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(format!("Cache body load error: {e}")),
        };

        let mut stmt = conn
            .prepare(
                "SELECT idx, filename, mime_type, data FROM attachments
                 WHERE envelope_hash = ?1 ORDER BY idx",
            )
            .map_err(|e| format!("Cache prepare error: {e}"))?;

        let rows = stmt
            .query_map([envelope_hash as i64], |row| {
                Ok(AttachmentData {
                    filename: row.get(1)?,
                    mime_type: row.get(2)?,
                    data: row.get(3)?,
                })
            })
            .map_err(|e| format!("Cache query error: {e}"))?;

        let mut attachments = Vec::new();
        for row in rows {
            attachments.push(row.map_err(|e| format!("Cache row error: {e}"))?);
        }

        Ok(Some((body_markdown, body_plain, attachments)))
    }

    fn do_save_body(
        conn: &Connection,
        envelope_hash: u64,
        body_markdown: &str,
        body_plain: &str,
        attachments: &[AttachmentData],
    ) -> Result<(), String> {
        let tx = conn
            .unchecked_transaction()
            .map_err(|e| format!("Cache tx error: {e}"))?;

        tx.execute(
            "UPDATE messages SET body_rendered = ?1, body_markdown = ?2 WHERE envelope_hash = ?3",
            rusqlite::params![body_plain, body_markdown, envelope_hash as i64],
        )
        .map_err(|e| format!("Cache body save error: {e}"))?;

        tx.execute(
            "DELETE FROM attachments WHERE envelope_hash = ?1",
            [envelope_hash as i64],
        )
        .map_err(|e| format!("Cache attachment delete error: {e}"))?;

        let mut stmt = tx
            .prepare(
                "INSERT INTO attachments (envelope_hash, idx, filename, mime_type, data)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )
            .map_err(|e| format!("Cache prepare error: {e}"))?;

        for (i, att) in attachments.iter().enumerate() {
            stmt.execute(rusqlite::params![
                envelope_hash as i64,
                i as i32,
                att.filename,
                att.mime_type,
                att.data,
            ])
            .map_err(|e| format!("Cache attachment insert error: {e}"))?;
        }
        drop(stmt);

        tx.commit()
            .map_err(|e| format!("Cache commit error: {e}"))?;
        Ok(())
    }

    // -- Phase 2b: dual-truth flag operations --------------------------------

    fn do_update_flags(
        conn: &Connection,
        envelope_hash: u64,
        flags_local: u8,
        pending_op: &str,
    ) -> Result<(), String> {
        let (is_read, is_starred) = flags_from_u8(flags_local);
        conn.execute(
            "UPDATE messages SET flags_local = ?1, pending_op = ?2, is_read = ?3, is_starred = ?4
             WHERE envelope_hash = ?5",
            rusqlite::params![
                flags_local as i32,
                pending_op,
                is_read as i32,
                is_starred as i32,
                envelope_hash as i64,
            ],
        )
        .map_err(|e| format!("Cache update_flags error: {e}"))?;
        Ok(())
    }

    fn do_clear_pending_op(
        conn: &Connection,
        envelope_hash: u64,
        flags_server: u8,
    ) -> Result<(), String> {
        let (is_read, is_starred) = flags_from_u8(flags_server);
        conn.execute(
            "UPDATE messages SET flags_server = ?1, flags_local = ?1, pending_op = NULL,
             is_read = ?2, is_starred = ?3
             WHERE envelope_hash = ?4",
            rusqlite::params![
                flags_server as i32,
                is_read as i32,
                is_starred as i32,
                envelope_hash as i64,
            ],
        )
        .map_err(|e| format!("Cache clear_pending error: {e}"))?;
        Ok(())
    }

    fn do_revert_pending_op(conn: &Connection, envelope_hash: u64) -> Result<(), String> {
        // Revert local flags to match server flags, clear pending
        conn.execute(
            "UPDATE messages SET flags_local = flags_server, pending_op = NULL,
             is_read = CASE WHEN (flags_server & 1) != 0 THEN 1 ELSE 0 END,
             is_starred = CASE WHEN (flags_server & 2) != 0 THEN 1 ELSE 0 END
             WHERE envelope_hash = ?1",
            [envelope_hash as i64],
        )
        .map_err(|e| format!("Cache revert_pending error: {e}"))?;
        Ok(())
    }

    fn do_remove_message(conn: &Connection, envelope_hash: u64) -> Result<(), String> {
        conn.execute(
            "DELETE FROM attachments WHERE envelope_hash = ?1",
            [envelope_hash as i64],
        )
        .map_err(|e| format!("Cache attachment cascade error: {e}"))?;

        conn.execute(
            "DELETE FROM messages WHERE envelope_hash = ?1",
            [envelope_hash as i64],
        )
        .map_err(|e| format!("Cache remove_message error: {e}"))?;
        Ok(())
    }

    fn do_search(conn: &Connection, query: &str) -> Result<Vec<MessageSummary>, String> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }

        let mut stmt = conn
            .prepare(
                "SELECT m.envelope_hash, m.subject, m.sender, m.date, m.timestamp,
                        m.is_read, m.is_starred, m.has_attachments, m.thread_id,
                        m.flags_server, m.flags_local, m.pending_op, m.mailbox_hash,
                        m.message_id, m.in_reply_to, m.thread_depth
                 FROM messages m
                 WHERE m.rowid IN (SELECT rowid FROM message_fts WHERE message_fts MATCH ?1)
                 ORDER BY m.timestamp DESC
                 LIMIT 200",
            )
            .map_err(|e| format!("Search prepare error: {e}"))?;

        let rows = stmt
            .query_map([query], |row| {
                let envelope_hash: i64 = row.get(0)?;
                let thread_id: Option<i64> = row.get(8)?;
                let flags_server: i32 = row.get::<_, Option<i32>>(9)?.unwrap_or(0);
                let flags_local: i32 = row.get::<_, Option<i32>>(10)?.unwrap_or(0);
                let pending_op: Option<String> = row.get(11)?;
                let mbox_hash: i64 = row.get(12)?;

                let effective_flags = if pending_op.is_some() {
                    flags_local as u8
                } else {
                    flags_server as u8
                };
                let (is_read, is_starred) = flags_from_u8(effective_flags);

                Ok(MessageSummary {
                    uid: envelope_hash as u64,
                    subject: row.get(1)?,
                    from: row.get(2)?,
                    date: row.get(3)?,
                    timestamp: row.get(4)?,
                    is_read,
                    is_starred,
                    has_attachments: row.get::<_, i32>(7)? != 0,
                    thread_id: thread_id.map(|t| t as u64),
                    envelope_hash: envelope_hash as u64,
                    mailbox_hash: mbox_hash as u64,
                    message_id: row.get::<_, Option<String>>(13)?.unwrap_or_default(),
                    in_reply_to: row.get(14)?,
                    thread_depth: row.get::<_, Option<u32>>(15)?.unwrap_or(0),
                })
            })
            .map_err(|e| format!("Search query error: {e}"))?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row.map_err(|e| format!("Search row error: {e}"))?);
        }
        Ok(results)
    }
}

/// Public constant for the default page size.
pub const DEFAULT_PAGE_SIZE: u32 = PAGE_SIZE;
