use rusqlite::Connection;

/// Schema DDL run on open.
pub(super) const SCHEMA: &str = "
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
pub(super) fn run_migrations(conn: &Connection) {
    let alters = [
        "ALTER TABLE messages ADD COLUMN flags_server INTEGER DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN flags_local INTEGER DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN pending_op TEXT",
        "ALTER TABLE messages ADD COLUMN message_id TEXT",
        "ALTER TABLE messages ADD COLUMN in_reply_to TEXT",
        "ALTER TABLE messages ADD COLUMN thread_depth INTEGER DEFAULT 0",
        "ALTER TABLE messages ADD COLUMN body_markdown TEXT",
        "ALTER TABLE messages ADD COLUMN reply_to TEXT",
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
