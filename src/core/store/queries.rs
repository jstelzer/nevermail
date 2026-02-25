use rusqlite::Connection;

use super::flags::{flags_from_u8, flags_to_u8};
use crate::core::models::{AttachmentData, Folder, MessageSummary};

/// Shared row-to-struct mapping for both `do_load_messages` and `do_search`.
///
/// Expects columns in this order:
///   0: envelope_hash, 1: subject, 2: sender, 3: date, 4: timestamp,
///   5: is_read, 6: is_starred, 7: has_attachments, 8: thread_id,
///   9: flags_server, 10: flags_local, 11: pending_op, 12: mailbox_hash,
///   13: message_id, 14: in_reply_to, 15: thread_depth, 16: reply_to
fn row_to_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageSummary> {
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
        reply_to: row.get(16)?,
    })
}

pub(super) fn do_save_folders(conn: &Connection, folders: &[Folder]) -> Result<(), String> {
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

pub(super) fn do_load_folders(conn: &Connection) -> Result<Vec<Folder>, String> {
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

pub(super) fn do_save_messages(
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
        for hash in rows.flatten() {
            pending_set.insert(hash as u64);
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
              message_id, in_reply_to, thread_depth, reply_to)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .map_err(|e| format!("Cache prepare error: {e}"))?;

    // For messages with pending ops, update only flags_server (not flags_local or pending_op)
    let mut update_server_stmt = tx
        .prepare(
            "UPDATE messages SET flags_server = ?1, subject = ?2, sender = ?3,
             date = ?4, timestamp = ?5, has_attachments = ?6, thread_id = ?7,
             message_id = ?8, in_reply_to = ?9, thread_depth = ?10, reply_to = ?11
             WHERE envelope_hash = ?12 AND pending_op IS NOT NULL",
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
                    m.reply_to,
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
                m.reply_to,
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

pub(super) fn do_load_messages(
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
                    message_id, in_reply_to, thread_depth, reply_to
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
            row_to_summary,
        )
        .map_err(|e| format!("Cache query error: {e}"))?;

    let mut messages = Vec::new();
    for row in rows {
        messages.push(row.map_err(|e| format!("Cache row error: {e}"))?);
    }
    Ok(messages)
}

pub(super) fn do_load_body(
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

pub(super) fn do_save_body(
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

pub(super) fn do_update_flags(
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

pub(super) fn do_clear_pending_op(
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

pub(super) fn do_revert_pending_op(conn: &Connection, envelope_hash: u64) -> Result<(), String> {
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

pub(super) fn do_remove_message(conn: &Connection, envelope_hash: u64) -> Result<(), String> {
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

pub(super) fn do_search(conn: &Connection, query: &str) -> Result<Vec<MessageSummary>, String> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }

    // Auto-append prefix wildcard to each token for plural tolerance
    // e.g. "goblin king" → "goblin* king*"
    // Skip if user is using explicit FTS syntax (quotes, operators)
    let fts_query: String = if query.contains('"') {
        query.to_string()
    } else {
        query
            .split_whitespace()
            .map(|token| {
                let is_plain =
                    token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !is_plain || token.len() < 3 || token.ends_with('*') {
                    token.to_string()
                } else {
                    format!("{token}*")
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };

    let mut stmt = conn
        .prepare(
            "SELECT m.envelope_hash, m.subject, m.sender, m.date, m.timestamp,
                    m.is_read, m.is_starred, m.has_attachments, m.thread_id,
                    m.flags_server, m.flags_local, m.pending_op, m.mailbox_hash,
                    m.message_id, m.in_reply_to, m.thread_depth, m.reply_to
             FROM messages m
             WHERE m.rowid IN (SELECT rowid FROM message_fts WHERE message_fts MATCH ?1)
             ORDER BY m.timestamp DESC
             LIMIT 200",
        )
        .map_err(|e| format!("Search prepare error: {e}"))?;

    let rows = stmt
        .query_map([&fts_query], row_to_summary)
        .map_err(|e| format!("Search query error: {e}"))?;

    let mut results = Vec::new();
    for row in rows {
        results.push(row.map_err(|e| format!("Search row error: {e}"))?);
    }
    Ok(results)
}
