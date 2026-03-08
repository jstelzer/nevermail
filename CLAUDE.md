# Claude Context: Neverlight Mail (COSMIC Desktop)

**Last Updated:** 2026-03-08

## What This Is

Neverlight Mail is a COSMIC desktop email client built on:
- **libcosmic** (git, HEAD) — COSMIC UI framework (iced fork)
- **neverlight-mail-core** — JMAP-native headless email engine (RFC 8620/8621). See [neverlight-mail-core/CLAUDE.md](../neverlight-mail-core/CLAUDE.md) for engine internals.

Target provider: Fastmail. Should work with any RFC 8620/8621 compliant JMAP server.

**This is a JMAP-only client.** No IMAP, no SMTP, no melib. Sending uses JMAP `EmailSubmission/set`, not SMTP. Push uses JMAP EventSource SSE, not IMAP IDLE.

## Read First

- `docs/code-conventions.md` — Code style, state modeling, error handling. **You must follow this.** It is shared with neverlight-mail-core and defines how we write Rust in this project: enums over boolean flags, `let-else` over nested `if let`, match once at the boundary, warnings are errors, no dead code, no `#[allow(...)]` without discussion.
- `neverlight-mail-core/CLAUDE.md` — Engine architecture, JMAP design rationale.

## Source of Truth

Use this file for architecture and implementation context only.

- User-facing behavior (features, setup, limitations): see [README.md](README.md)
- Provider-specific guidance and caveats: see files under [`docs/`](docs/)

## Architecture

- **neverlight-mail-core** — Headless JMAP engine (zero COSMIC deps). Library crate.
- **neverlight-mail** (root) — COSMIC desktop GUI. Binary crate, depends on neverlight-mail-core.

```
neverlight-mail/                    (workspace root)
├── Cargo.toml                      (workspace + GUI binary package)
├── Cargo.lock                      (shared lockfile)
├── neverlight-mail-core/
│   ├── Cargo.toml                  (lib: reqwest, mail-parser, rusqlite, keyring, ...)
│   ├── src/
│   │   ├── lib.rs                  (pub mod + type re-exports)
│   │   ├── types.rs               — EmailId, MailboxId, Flags, FlagOp, SyncEvent
│   │   ├── client.rs              — JmapClient: HTTP transport, request batching
│   │   ├── session.rs             — JMAP session discovery, capability negotiation
│   │   ├── config.rs              — Config resolution (env → file+keyring → setup dialog)
│   │   ├── email.rs               — Email/query, Email/get, Email/set
│   │   ├── mailbox.rs             — Mailbox/get, find_by_role
│   │   ├── submit.rs              — EmailSubmission/set (replaces SMTP)
│   │   ├── push.rs                — EventSource SSE (replaces IMAP IDLE)
│   │   ├── parse.rs               — RFC 5322 body extraction via mail-parser
│   │   ├── mime.rs                — render_body, render_body_markdown, open_link
│   │   ├── keyring.rs             — OS keyring credential backend
│   │   ├── models.rs              — Folder, MessageSummary, AttachmentData
│   │   ├── setup.rs               — UI-agnostic setup state machine
│   │   └── store/
│   │       ├── mod.rs             — Re-exports (CacheHandle, flags_to_u8, DEFAULT_PAGE_SIZE)
│   │       ├── schema.rs          — DDL + forward-only migrations + FTS5 setup
│   │       ├── flags.rs           — Flag encode/decode (compact 2-bit encoding)
│   │       ├── commands.rs        — CacheCmd enum (channel message types)
│   │       ├── queries.rs         — All do_* SQL functions + shared row_to_summary
│   │       └── handle.rs          — CacheHandle async facade + background thread
│   └── tests/fixtures/             — Test email fixtures
├── src/                            (GUI binary crate)
│   ├── main.rs                    — Entry point, env_logger init, cosmic::app::run
│   ├── dnd_models.rs              — DraggedFiles, DraggedMessage (COSMIC DnD types)
│   ├── app/
│   │   ├── mod.rs                 — AppModel struct, Message enum, trait impl, dispatcher
│   │   ├── accounts.rs            — Account state management, folder revalidation
│   │   ├── actions.rs             — Flag/move handlers (toggle read/star, trash, archive)
│   │   ├── body.rs                — Body/attachment viewing handlers
│   │   ├── compose.rs             — Compose handlers + quote/forward helpers
│   │   ├── layout.rs              — Pane layout persistence
│   │   ├── navigation.rs          — Keyboard nav + recompute_visible()
│   │   ├── search.rs              — FTS search handlers
│   │   ├── setup.rs               — Setup dialog handlers + view builder
│   │   ├── sync.rs                — Connection/sync/folder handlers
│   │   └── watch.rs               — JMAP EventSource push + event handlers
│   └── ui/
│       ├── sidebar.rs             — Folder list + diagnostics panel
│       ├── message_list.rs        — Message header list + search bar
│       ├── message_view.rs        — Message body preview pane
│       └── compose_dialog.rs      — Compose/reply/forward dialog
└── .github/workflows/ci.yml
```

**Import conventions:** GUI code imports from `neverlight_mail_core::` (config, client, email, mailbox, models, store, etc.) and `crate::` (dnd_models, app, ui). Core re-exports key types (`EmailId`, `MailboxId`, `Flags`, `FlagOp`) so the GUI uses string-based IDs throughout.

**ID types:** All identifiers are strings. `email_id: String` (JMAP Email ID), `mailbox_id: String` (JMAP Mailbox ID), `thread_id: Option<String>` (JMAP Thread ID), `account_id: String` (local UUID). No u64 hashes anywhere.

## Design Principles

### Split by direction, not by feature
Organize modules by *who calls whom*, not by domain noun. The app layer dispatches messages to handler modules (`sync.rs`, `actions.rs`, `compose.rs`, etc.) that each own a slice of the update logic. The core layer provides services (`email`, `mailbox`, `submit`, `store`) that handlers call into. UI modules are pure view functions that take state and return elements. No layer reaches upward.

### JMAP-only, no protocol abstraction
There is no `MailBackend` trait, no `MailSession` enum, no protocol dispatch. The app talks directly to `JmapClient`. If you need IMAP, use the `main` branch.

### Push arg lists into request/command structs
When a function takes 4+ related parameters, collapse them into a struct. `CacheCmd` already does this for store operations. `SendRequest` does this for email submission.

### Friction-driven polish
Only fix things that annoy you while actually reading mail. One commit per annoyance. This prevents rewrite spirals and keeps effort proportional to real pain.

## Key Design Decisions

### COSMIC Task Pattern
COSMIC's `Task<M>` is `iced::Task<cosmic::Action<M>>`. You cannot use `Task::perform()` directly with app messages. Use `cosmic::task::future()` instead:
```rust
cosmic::task::future(async move {
    let result = neverlight_mail_core::mailbox::fetch_all(&client).await;
    Message::SyncFoldersComplete { account_id, epoch, result }
})
```

### Connection Flow
```
init() → resolve_all_accounts() → JmapSession::connect(&config)
  → AccountConnected(Ok(JmapClient)) → fetch_all(&client)
    → SyncFoldersComplete(Ok) → auto-select INBOX → query_and_get(&client, &mailbox_id)
      → SyncMessagesComplete(Ok) → display in list

SelectFolder(i) → query_and_get(&client, &mailbox_id, page_size, offset)
ViewBody(i) → get_body(&client, &email_id) → render via markdown
```

### Core API Surface (what the GUI calls)
```rust
// Folders
mailbox::fetch_all(&client) -> Result<Vec<Folder>>
mailbox::find_by_role(&folders, "trash") -> Option<String>  // returns mailbox_id

// Messages
email::query_and_get(&client, &mailbox_id, page_size, offset) -> Result<(Vec<MessageSummary>, ...)>
email::get_body(&client, &email_id) -> Result<(String, String, Vec<AttachmentData>)>
email::set_flag(&client, &email_id, &FlagOp) -> Result<()>
email::move_to(&client, &email_id, &from_mailbox, &to_mailbox) -> Result<()>
email::trash(&client, &email_id, &current_mailbox, &trash_mailbox) -> Result<()>

// Sending (replaces SMTP)
submit::get_identities(&client) -> Result<Vec<Identity>>
submit::send(&client, &SendRequest) -> Result<String>

// Push (replaces IMAP IDLE)
push::listen(&client, &EventSourceConfig, on_change_callback) -> Result<()>

// Session
session::JmapSession::connect(&config) -> Result<(JmapSession, JmapClient)>
```

### Optimistic Updates & Rollback
Flag toggles and message moves apply immediately in the UI, then confirm with the server async. On failure the UI reverts:
- **Flags:** `FlagOpComplete` carries `prev_flags` (compact 2-bit via `flags_to_u8`). Failure restores exact pre-op read+star state.
- **Moves:** `remove_message_optimistic()` returns the removed `MessageSummary`; callers stash it in `pending_move_restore` keyed by `MessageIdentity`. `MoveOpComplete(Err)` re-inserts at the original index and repairs selection. JMAP `Email/set` is atomic — no postcondition refetch needed (unlike IMAP MOVE).
- **Selection:** `remove_message_optimistic` decrements selection when the removed row was above, clamps when it was the selected row, and clears the preview pane only when needed.

### Lane Epochs (Stale Apply Protection)
Async completions carry lane epochs so stale results are dropped instead of mutating current state:
- **Folder lane:** `CachedMessagesLoaded { account_id, mailbox_id, offset, epoch, ... }`
- **Message lane:** `SyncMessagesComplete { account_id, mailbox_id, epoch, ... }`
- **Search lane:** `SearchResultsLoaded { query, epoch, ... }`
- **Flag lane:** per-message latest epoch tracked in `pending_flag_epochs`
- **Mutation lane:** per-message latest epoch tracked in `pending_move_epochs`

When epoch/context mismatch is detected, the apply is ignored and `stale_apply_drop_count` increments.

Lane operations also use explicit abort handles for supersession:
- `search_abort` cancels prior in-flight search task when a newer search starts
- `folder_abort` cancels superseded cache TOC loads
- `message_abort` cancels superseded message fetches
- `body_abort` cancels superseded body fetches

### Refresh Lane Coalescing
`Refresh` requests are coalesced:
- if a refresh is already running, new `Refresh` intents set `refresh_pending = true` and do not start overlapping work
- when the current refresh finishes (`refresh_accounts_outstanding` drains), one queued refresh starts if pending
- if a refresh appears stuck (45s timeout), `refresh_in_flight` is force-cleared, epoch bumped, and a new refresh starts immediately

### Selection Revalidation
`recompute_visible()` always calls centralized selection revalidation:
- selected index must exist in canonical TOC
- selected row must remain visible in current thread-collapse projection
- when invalid, selection is clamped/cleared and preview state is reset

### Domain Phase
App state tracks domain phase explicitly via `Phase`:
- `Idle`, `Loading`, `Refreshing`, `Searching`, `Error`

Handlers update phase during mailbox loads, refresh, search, and failure paths so snapshot consumers can render from state instead of inferring from ad hoc flags.

### Setup Dialog
JMAP-only, 5 fields: Label, JMAP Session URL, Username, Token (app password), Email addresses.
The core's `SetupModel` (in `setup.rs`) owns validation and persistence. The GUI maps COSMIC widget events to `SetupInput::SetField(FieldId::JmapUrl, value)` etc. Three modes: `Full` (new account), `TokenOnly` (re-enter token), `Edit` (modify existing).

## Sharp Edges: Wayland / COSMIC DnD

These are hard-won lessons. Do not re-learn them.

**Dialog overlays don't support DnD.** COSMIC's `dialog()` renders as a modal overlay. Widgets inside overlays don't call `drag_destinations`, so `dnd_destination` widgets placed there are invisible to the Wayland compositor. Files snap back to the file manager with no error. Fix: DnD destinations must live in the main `view()`, never inside `dialog()`.

**File managers use the xdg document portal, not `text/uri-list`.** On Wayland, dragged files arrive as `application/vnd.portal.filetransfer` with a transfer key. You must chain `.on_file_transfer()` on `dnd_destination` and resolve paths via `ashpd::documents::FileTransfer::retrieve_files(key)`. The `text/uri-list` codepath exists as fallback for X11/non-portal apps only.

**Current DnD architecture:**
- File drop `dnd_destination` wraps the entire `view()` output in `app/mod.rs`
- Portal path: `ComposeFileTransfer(key)` → `ComposeFileTransferResolved(paths)` → `ComposeAttachLoaded`
- URI-list fallback: `ComposeFilesDropped(DraggedFiles)` → parse → `ComposeAttachLoaded`
- Message-to-folder: `dnd_source` on message rows, `dnd_destination` on sidebar folders (both in main view, works fine)
- `actions.rs` has shared `remove_message_optimistic()` + `dispatch_move()` for Trash/Archive/DnD moves

## Connection Health & Reconnect

**Push stream lifecycle:** JMAP EventSource (SSE) replaces IMAP IDLE. Each connected account gets a `push_watch_stream` subscription that calls `push::listen()`. When the SSE response ends or errors, the GUI schedules a 5s-delayed `ForceReconnect`.

**Defenses (sync.rs, watch.rs):**
- `SyncFoldersComplete(Err)` and `SyncMessagesComplete(Err)` drop the client, set `conn_state = Error`, and schedule reconnect — sync failures are treated as connection failures
- `AccountConnected(Err)` schedules retry with exponential backoff (5s → 15s → 30s → 60s cap) via `AccountState::reconnect_backoff()`. Counter resets on success.
- Stuck refresh (45s timeout) force-clears `refresh_in_flight` + bumps epoch so stale completions are dropped, then starts a new refresh immediately
- `PushError` / `PushEnded` set `last_error` for diagnostics and trigger 5s-delayed reconnect

**neverlight-mail-core boundary:** The core is shared across multiple clients. Connection health policy (backoff, session invalidation, stuck detection) belongs in each client's app layer, not in the core.

**Manual refresh:** F5 triggers `Message::Refresh`. Status pill click sends `Refresh` when Connected, `ForceReconnect` when Error/Disconnected.

**Diagnostics panel** (sidebar, collapsible): per-account connection state + reconnect attempt count, last refresh/sync timing, refresh-in-flight indicator, non-zero anomaly counters only.

## Known Limitations

- **App password auth only** — no OAuth; Fastmail app passwords work
- **No mailbox management** — create/rename/delete mailboxes not supported
- **Fastmail validated only** — other JMAP providers may work but are untested
- **No offline compose** — requires active JMAP client for identity resolution and submission
