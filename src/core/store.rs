use std::path::PathBuf;

use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

use crate::core::models::{Folder, MessageSummary};

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
";

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
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    SaveBody {
        envelope_hash: u64,
        body: String,
        reply: oneshot::Sender<Result<(), String>>,
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

    pub async fn load_body(&self, envelope_hash: u64) -> Result<Option<String>, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::LoadBody {
                envelope_hash,
                reply,
            })
            .map_err(|_| "Cache unavailable".to_string())?;
        rx.await.map_err(|_| "Cache unavailable".to_string())?
    }

    pub async fn save_body(&self, envelope_hash: u64, body: String) -> Result<(), String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(CacheCmd::SaveBody {
                envelope_hash,
                body,
                reply,
            })
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
                    body,
                    reply,
                } => {
                    let _ = reply.send(Self::do_save_body(&conn, envelope_hash, &body));
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

        // Write-through: replace all messages for this mailbox
        tx.execute(
            "DELETE FROM messages WHERE mailbox_hash = ?1",
            [mailbox_hash as i64],
        )
        .map_err(|e| format!("Cache delete error: {e}"))?;

        let mut stmt = tx
            .prepare(
                "INSERT INTO messages (envelope_hash, mailbox_hash, subject, sender, date, timestamp, is_read, is_starred, has_attachments, thread_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )
            .map_err(|e| format!("Cache prepare error: {e}"))?;

        for m in messages {
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
            ])
            .map_err(|e| format!("Cache insert error: {e}"))?;
        }
        drop(stmt);

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
                "SELECT envelope_hash, subject, sender, date, timestamp, is_read, is_starred, has_attachments, thread_id
                 FROM messages
                 WHERE mailbox_hash = ?1
                 ORDER BY timestamp DESC
                 LIMIT ?2 OFFSET ?3",
            )
            .map_err(|e| format!("Cache prepare error: {e}"))?;

        let rows = stmt
            .query_map(
                rusqlite::params![mailbox_hash as i64, limit, offset],
                |row| {
                    let envelope_hash: i64 = row.get(0)?;
                    let thread_id: Option<i64> = row.get(8)?;
                    Ok(MessageSummary {
                        uid: envelope_hash as u64,
                        subject: row.get(1)?,
                        from: row.get(2)?,
                        date: row.get(3)?,
                        timestamp: row.get(4)?,
                        is_read: row.get::<_, i32>(5)? != 0,
                        is_starred: row.get::<_, i32>(6)? != 0,
                        has_attachments: row.get::<_, i32>(7)? != 0,
                        thread_id: thread_id.map(|t| t as u64),
                        envelope_hash: envelope_hash as u64,
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

    fn do_load_body(conn: &Connection, envelope_hash: u64) -> Result<Option<String>, String> {
        let result = conn.query_row(
            "SELECT body_rendered FROM messages WHERE envelope_hash = ?1",
            [envelope_hash as i64],
            |row| row.get::<_, Option<String>>(0),
        );

        match result {
            Ok(body) => Ok(body),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(format!("Cache body load error: {e}")),
        }
    }

    fn do_save_body(conn: &Connection, envelope_hash: u64, body: &str) -> Result<(), String> {
        conn.execute(
            "UPDATE messages SET body_rendered = ?1 WHERE envelope_hash = ?2",
            rusqlite::params![body, envelope_hash as i64],
        )
        .map_err(|e| format!("Cache body save error: {e}"))?;
        Ok(())
    }
}

/// Public constant for the default page size.
pub const DEFAULT_PAGE_SIZE: u32 = PAGE_SIZE;
