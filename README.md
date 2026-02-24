# Nevermail

A COSMIC desktop email client for Linux, built in Rust.

**Status:** Early alpha — connects, caches, sends, renders HTML. Single account.

## Stack

| Component   | Crate                                                       | Role                                      |
|-------------|-------------------------------------------------------------|-------------------------------------------|
| UI          | [libcosmic](https://github.com/pop-os/libcosmic)            | COSMIC desktop toolkit (iced-based)       |
| Mail engine | [melib](https://crates.io/crates/melib)                     | IMAP, MIME parsing, envelope handling     |
| SMTP        | [lettre](https://crates.io/crates/lettre)                   | Outbound mail delivery (not yet wired)    |
| HTML render | [html2md](https://crates.io/crates/html2md) + iced markdown | HTML → markdown → native rich text        |
| Sanitizer   | [ammonia](https://crates.io/crates/ammonia)                 | Strip email layout junk before conversion |
| Plaintext   | [html2text](https://crates.io/crates/html2text)             | Plain-text fallback for quoting/FTS       |
| Cache       | [rusqlite](https://crates.io/crates/rusqlite)               | Local SQLite message cache                |
| Credentials | [keyring](https://crates.io/crates/keyring)                 | OS keyring (libsecret/gnome-keyring)      |
| Async       | [tokio](https://crates.io/crates/tokio)                     | Async runtime                             |

## Architecture

```
src/
├── main.rs          Entry point
├── app.rs           COSMIC Application (MVU model + update + view + dialog)
├── config.rs        Account config: env vars → config file + keyring → setup dialog
├── core/
│   ├── models.rs    Domain types (Folder, MessageSummary, MessageBody)
│   ├── imap.rs      melib IMAP wrapper
│   ├── store.rs     SQLite cache layer (pagination, body cache)
│   ├── keyring.rs   OS keyring wrapper (get/set/delete password)
│   └── mime.rs      HTML-to-text rendering, link handling
└── ui/
    ├── sidebar.rs       Folder list
    ├── message_list.rs  Message header list
    └── message_view.rs  Message body preview
```

The app follows the COSMIC MVU (Model-View-Update) pattern:
- **Model**: `AppModel` holds all state (folders, messages, selection, sync status, dialog)
- **View**: Three-pane layout (sidebar | message list | message preview)
- **Update**: `Message` enum drives all state transitions — UI widgets never call core directly

Data flows: IMAP (via melib) → domain models → SQLite cache → COSMIC widgets.

### Design principles

- **Cache is the UI source of truth.** The list renders from SQLite, not from live IMAP state. Background sync updates the cache, then the UI refreshes from it.
- **`core/` stays UI-independent.** No COSMIC types leak into the mail engine. Someone should be able to `use nevermail::core::*` without pulling in a GUI framework.
- **Credentials resolve gracefully.** Env vars override everything. Config file + keyring is the default path. Missing credentials show a dialog — no panics.

## Roadmap

- [x] **Phase 0**: Connect to IMAP, list folders, display message headers
- [x] **Phase 1**: SQLite cache, incremental sync, pagination, body preview
- [x] **Phase 2a**: Credential management (config file + OS keyring + setup dialog)
- [x] **Phase 2b**: Flags + actions (seen/star toggles, archive, delete, move)
- [x] **Phase 2c**: Threading (compute from headers, store in cache, render with indentation)
- [x] **Phase 2d**: Keyboard shortcuts (j/k navigation, action keys, thread collapse)
- [x] **Phase 3**: Compose + send (SMTP via lettre)
- [x] **Phase 4**: Attachments/Download
- [x] **Phase 5**: Support multiple from addrs
- [x] **Phase 6**: Background task / notifications 
- [x] **Phase 7**: Figure out html rendering and consider adding https://github.com/Mrmayman/frostmark  / FTS
- [ ] **Phase 8**: Drag & Drop
- [ ] **Phase 9**: Allow smtp creds to be distinct from imap
- [ ] **Phase 10**: OAuth2, multiple accounts

### Phase 7 -- context
It's better not have to a full web engine in an email client. Converting HTML into a rich text widget (which iced does officially support) is all that's truly necessary.

3h ago
We are doing similar for appstream metadata in the COSMIC Store. They use HTML markup for their store pages and we simply convert that into a native interface. 

Frostmark is pinned to a different version of iced.

### Phase 2b–d design notes

**Flags** use a dual-truth model to prevent sync races: `flags_server` (from IMAP) + `flags_local_overrides` (from pending user actions). Effective flags = server flags patched by local overrides until the IMAP op confirms. This prevents "I starred it and it instantly unstarred" when a background sync arrives.

**Actions** (archive, delete, move) go through an optimistic UI queue: update the UI immediately, write to cache, enqueue the IMAP operation, reconcile on success/failure. A `pending_ops` queue in the model keeps MVU clean.

**Threading** is computed in the cache layer from `Message-ID`, `In-Reply-To`, and `References` headers — not in the view. Thread metadata (thread_id, depth, order_key) lives in SQLite so the list can render without recomputing every frame.

### OAuth2

Nevermail uses password authentication (stored in the OS keyring). OAuth2 is not supported.

Gmail requires the `https://mail.google.com/` scope for IMAP — a **restricted scope** under Google's verification policy. That means:

- A third-party security audit before approval
- Annual re-verification
- The audit and process are designed for companies, not indie projects

Thunderbird and Outlook ship pre-approved client IDs because Mozilla and Microsoft can absorb that overhead. Smaller open source clients either ask users to create their own Google Cloud credentials (each user runs as a "test app" — functional but ugly UX), or they just don't bother.

For now, nevermail targets standard IMAP providers (Runbox, Fastmail, Migadu, self-hosted, etc.) that work with normal credentials. If the project ever reaches a scale where Google verification is worth pursuing, the Rust ecosystem is ready — [`oauth2-rs`](https://github.com/ramosbugs/oauth2-rs) handles PKCE flows and token refresh, and melib already supports XOAUTH2 in its account config. The plumbing isn't hard; the bureaucracy is.

## Not yet supported

- Multiple accounts
- Drag and drop

## Building

Requires Rust and system dependencies for libcosmic (Wayland dev libraries).

```sh
cargo build            # debug (large, ~600M+ — normal for wgpu debug builds)
cargo build --release  # release (~44M)
```

## Configuration

On first run, a setup dialog prompts for IMAP server, username, and password. Credentials are stored in the OS keyring (gnome-keyring/libsecret) with a config file at `~/.config/nevermail/config.json`.

Environment variables override everything (useful for development/testing):

```sh
export NEVERMAIL_SERVER=mail.runbox.com
export NEVERMAIL_PORT=993
export NEVERMAIL_USER=you@runbox.com
export NEVERMAIL_PASSWORD=yourpassword
export NEVERMAIL_STARTTLS=false
```

## License

Apache-2.0/MIT
