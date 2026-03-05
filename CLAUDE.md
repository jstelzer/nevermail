# Claude Context: Neverlight Mail (cosmic-email)

**Last Updated:** 2026-03-05

## What This Is

Neverlight Mail is a COSMIC desktop email client built on:
- **libcosmic** (git, HEAD) — COSMIC UI framework (iced fork)
- **neverlight-mail-core** — Headless email engine (IMAP, SMTP, MIME, cache). See [neverlight-mail-core/CLAUDE.md](../neverlight-mail-core/CLAUDE.md) for engine internals and version pinning.

Target server: Runbox (mail.runbox.com:993, implicit TLS). Should work with any standard IMAP server.

## Source of Truth

Use this file for architecture and implementation context only.

- User-facing behavior (features, setup, limitations): see [README.md](README.md)
- Provider-specific guidance and caveats: see files under [`docs/`](docs/)
- Prioritized backlog and implementation planning: see [BACKLOG.md](BACKLOG.md)

## Architecture

- **neverlight-mail-core** — Headless email engine (zero COSMIC deps). Library crate.
- **neverlight-mail** (root) — COSMIC desktop GUI. Binary crate, depends on neverlight-mail-core.

```
neverlight-mail/                    (workspace root)
├── Cargo.toml                      (workspace + GUI binary package)
├── Cargo.lock                      (shared lockfile)
├── neverlight-mail-core/
│   ├── Cargo.toml                  (lib: melib, lettre, rusqlite, keyring, ...)
│   ├── src/
│   │   ├── lib.rs                  (pub mod + melib re-exports)
│   │   ├── config.rs              — Config resolution (env → file+keyring → setup dialog)
│   │   ├── imap.rs                — ImapSession: connect, fetch, flags, move, watch
│   │   ├── smtp.rs                — SMTP send via lettre
│   │   ├── mime.rs                — render_body, clean_email_html, open_link
│   │   ├── keyring.rs             — OS keyring credential backend
│   │   ├── models.rs              — Folder, MessageSummary, AttachmentData
│   │   └── store/
│   │       ├── mod.rs             — Re-exports (CacheHandle, flags_to_u8, DEFAULT_PAGE_SIZE)
│   │       ├── schema.rs          — DDL + forward-only migrations + FTS5 setup
│   │       ├── flags.rs           — Flag encode/decode (compact 2-bit encoding)
│   │       ├── commands.rs        — CacheCmd enum (channel message types)
│   │       ├── queries.rs         — All do_* SQL functions + shared row_to_summary
│   │       └── handle.rs          — CacheHandle async facade + background thread run_loop
│   └── tests/fixtures/             — Test email fixtures
├── src/                            (GUI binary crate)
│   ├── main.rs                    — Entry point, env_logger init, cosmic::app::run
│   ├── dnd_models.rs              — DraggedFiles, DraggedMessage (COSMIC DnD types)
│   ├── app/
│   │   ├── mod.rs                 — AppModel struct, Message enum, trait impl, dispatcher
│   │   ├── actions.rs             — Flag/move handlers (toggle read/star, trash, archive)
│   │   ├── body.rs                — Body/attachment viewing handlers
│   │   ├── compose.rs             — Compose handlers + quote/forward helpers
│   │   ├── navigation.rs          — Keyboard nav + recompute_visible()
│   │   ├── search.rs              — FTS search handlers
│   │   ├── setup.rs               — Setup dialog handlers + view builder
│   │   ├── sync.rs                — Connection/sync/folder handlers
│   │   └── watch.rs               — IMAP IDLE watch stream + event handlers
│   └── ui/
│       ├── sidebar.rs             — Folder list view
│       ├── message_list.rs        — Message header list + search bar
│       ├── message_view.rs        — Message body preview pane
│       └── compose_dialog.rs      — Compose/reply/forward dialog
└── .github/workflows/ci.yml
```

**Import conventions:** GUI code imports from `neverlight_mail_core::` (config, imap, models, store, etc.) and `crate::` (dnd_models, app, ui). Core re-exports key melib types (`EnvelopeHash`, `MailboxHash`, `FlagOp`, `Flag`, `BackendEvent`, `RefreshEventKind`) so the GUI never depends on melib directly.

## Design Principles

### Split by direction, not by feature
Organize modules by *who calls whom*, not by domain noun. The app layer dispatches messages to handler modules (`sync.rs`, `actions.rs`, `compose.rs`, etc.) that each own a slice of the update logic. The core layer provides services (`imap`, `smtp`, `store`) that handlers call into. UI modules are pure view functions that take state and return elements. No layer reaches upward.

### Traits only at real seam points
Don't trait-everything. Add port traits (`MailBackend`, `SendBackend`, `SecretStore`) only when there's a concrete second consumer — testability or an alternate implementation. Right now `ImapSession` is the only mail backend and `keyring.rs` is small; speculative abstraction adds complexity without payoff.

### Push arg lists into request/command structs
When a function takes 4+ related parameters, collapse them into a struct. `CacheCmd` already does this for store operations. Extend the pattern outward when IMAP or SMTP call sites get unwieldy.

### Friction-driven polish
Only fix things that annoy you while actually reading mail. One commit per annoyance. This prevents rewrite spirals and keeps effort proportional to real pain.

## Key Design Decisions

### COSMIC Task Pattern
COSMIC's `Task<M>` is `iced::Task<cosmic::Action<M>>`. You cannot use `Task::perform()` directly with app messages. Use `cosmic::task::future()` instead, which auto-wraps via the blanket `impl<M> From<M> for Action<M>`:
```rust
cosmic::task::future(async move {
    Message::FoldersLoaded(session.fetch_folders().await)
})
```

### ImapSession Design
- Wraps `Arc<Mutex<Box<ImapType>>>` for interior mutability (`fetch()` requires `&mut self`)
- `ImapSession` itself lives behind `Arc<ImapSession>` so it can be cloned into async tasks
- melib's `ResultFuture<T>` is `Result<BoxFuture<'static, Result<T>>>` — double-unwrap pattern:
  ```rust
  let future = backend.mailboxes().map_err(/*...*/)?;  // outer Result
  let result = future.await.map_err(/*...*/)?;          // inner Result
  ```
- Streams from `fetch()` are `'static` — safe to drop the lock before consuming

### Async Flow
```
init() → Config::from_env() → ImapSession::connect()
  → Connected(Ok) → fetch_folders()
    → FoldersLoaded(Ok) → auto-select INBOX → fetch_messages()
      → MessagesLoaded(Ok) → display in list

SelectFolder(i) → fetch_messages(mailbox_hash)
SelectMessage(i) → fetch_body(envelope_hash) → render via mime::render_body()
```

### Optimistic Updates & Rollback
Flag toggles and message moves apply immediately in the UI, then confirm with the server async. On failure the UI reverts:
- **Flags:** `FlagOpComplete` carries `prev_flags` (compact 2-bit via `flags_to_u8`). Failure restores exact pre-op read+star state.
- **Moves:** `remove_message_optimistic()` returns the removed `MessageSummary`; callers stash it in `pending_move_restore` keyed by envelope hash. `MoveOpComplete(Err)` re-inserts at the original index and repairs selection. Success clears the snapshot.
- **Selection:** `remove_message_optimistic` decrements selection when the removed row was above, clamps when it was the selected row, and clears the preview pane only when needed.

### Lane Epochs (Stale Apply Protection)
Async completions carry lane epochs so stale results are dropped instead of mutating current state:
- **Folder lane:** `CachedMessagesLoaded { account_id, mailbox_hash, offset, epoch, ... }`
- **Message lane:** `SyncMessagesComplete { account_id, mailbox_hash, epoch, ... }`
- **Search lane:** `SearchResultsLoaded { query, epoch, ... }`
- **Flag lane:** per-envelope latest epoch tracked in `pending_flag_epochs`
- **Mutation lane:** per-envelope latest epoch tracked in `pending_move_epochs`

When epoch/context mismatch is detected, the apply is ignored and `stale_apply_drop_count` increments.

Lane operations also use explicit abort handles for supersession:
- `search_abort` cancels prior in-flight search task when a newer search starts
- `folder_abort` cancels superseded cache TOC loads
- `message_abort` cancels superseded IMAP message fetches

This means newer intents both cancel prior work and still keep stale-apply guards as a safety net.

### Refresh Lane Coalescing
`Refresh` requests are now coalesced:
- if a refresh is already running, new `Refresh` intents set `refresh_pending = true` and do not start overlapping work
- when the current refresh finishes (`refresh_accounts_outstanding` drains), one queued refresh starts if pending
- if a refresh appears stuck (45s timeout), `refresh_in_flight` is force-cleared, epoch bumped, and a new refresh starts immediately
- this enforces at-most-one active refresh operation while still honoring the latest intent

### Move Postcondition Reconcile
After `MoveOpComplete(Ok)`, the app verifies the source mailbox no longer contains the moved envelope by refetching source TOC from IMAP:
- success (`true`) => no-op
- failure (`false`) => increment `postcondition_failure_count` + `toc_drift_count`, emit retryable status, trigger `Refresh`
- check error => emit retryable status and trigger `Refresh`

This keeps canonical TOC convergence explicit when server-side MOVE behavior drifts.

### Selection Revalidation
`recompute_visible()` now always calls centralized selection revalidation:
- selected index must exist in canonical TOC
- selected row must remain visible in current thread-collapse projection
- when invalid, selection is clamped/cleared and preview state is reset

### Domain Phase
App state now tracks domain phase explicitly via `Phase`:
- `Idle`
- `Loading`
- `Refreshing`
- `Searching`
- `Error`

Handlers update phase during mailbox loads, refresh, search, and failure paths so snapshot consumers can render from state instead of inferring from ad hoc flags.

### MIME Body Extraction
Walks the attachment tree recursively looking for text/plain and text/html parts. Uses `Attachment::decode(Default::default())` for content-transfer-encoding. Prefers text/plain; falls back to html2text on text/html.

## Configuration and Credentials

For current setup flow, config behavior, keyring usage, and environment overrides, refer to [README.md](README.md). Keep this document focused on architecture decisions to avoid behavior drift.

## melib API Quick Reference

Key types and their locations in melib 0.8.13:
- `AccountSettings` — `melib::conf`, extra config via `IndexMap<String, String>` (flattened serde)
- `ImapType::new(&AccountSettings, IsSubscribedFn, BackendEventConsumer) -> Result<Box<Self>>`
- `MailBackend::mailboxes() -> ResultFuture<HashMap<MailboxHash, Mailbox>>`
- `MailBackend::fetch(&mut self, MailboxHash) -> ResultStream<Vec<Envelope>>`
- `MailBackend::envelope_bytes_by_hash(EnvelopeHash) -> ResultFuture<Vec<u8>>`
- `Mail::new(bytes, flags) -> Result<Mail>` then `mail.body() -> Attachment`
- `Envelope`: `.subject()`, `.from()`, `.date_as_str()`, `.is_seen()`, `.flags().is_flagged()`, `.has_attachments`, `.hash() -> EnvelopeHash`
- `BackendMailbox` (trait behind `Mailbox` type alias): `.name()`, `.path()`, `.count() -> (total, unseen)`, `.hash()`
- `IsSubscribedFn`: `Arc<dyn Fn(&str) -> bool + Send + Sync>.into()`
- `BackendEventConsumer::new(Arc<dyn Fn(AccountHash, BackendEvent) + Send + Sync>)`
- Hash types: `MailboxHash(pub u64)`, `EnvelopeHash(pub u64)` — transparent newtypes

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

**The dead-session cascade** is the primary reliability hazard. melib's IDLE stream can go silent on a dead TCP connection (no error, no stream end — just stuck). The core (`imap.rs`) is a thin passthrough over melib and intentionally does not own connection health policy. All liveness detection and recovery lives in the GUI layer.

The cascade before fixes: IDLE dies silently → 5-min periodic refresh reuses dead session → `SyncFoldersComplete(Err)` → conn_state set to "Connected" (masking the dead session) → next refresh repeats → client appears stuck until restart.

**Current defenses (sync.rs, watch.rs):**
- `SyncFoldersComplete(Err)` and `SyncMessagesComplete(Err)` now drop the session, set `conn_state = Error`, and schedule reconnect — sync failures are treated as connection failures
- `AccountConnected(Err)` schedules retry with exponential backoff (5s → 15s → 30s → 60s cap) via `AccountState::reconnect_backoff()`. Counter resets on success.
- Stuck refresh (45s timeout) force-clears `refresh_in_flight` + bumps epoch so stale completions are dropped, then starts a new refresh immediately
- `WatchError` / `WatchEnded` set `last_error` for diagnostics and trigger 5s-delayed reconnect (which chains into the backoff retry on failure)

**neverlight-mail-core boundary:** The core is shared across multiple clients. Connection health policy (backoff, session invalidation, stuck detection) belongs in each client's app layer, not in the core. The core's `ImapSession::watch()` returns melib's stream as-is.

**Manual refresh:** F5 triggers `Message::Refresh`. Status pill click sends `Refresh` when Connected, `ForceReconnect` when Error/Disconnected.

**Diagnostics panel** (sidebar, collapsible): per-account connection state + reconnect attempt count, last refresh/sync timing, refresh-in-flight indicator, non-zero anomaly counters only.

## Known Limitations

- **No offline compose** — requires active IMAP session for SMTP relay config
