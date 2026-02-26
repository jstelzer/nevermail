# Claude Context: Nevermail (cosmic-email)

**Last Updated:** 2026-02-25

## What This Is

Nevermail is a COSMIC desktop email client built on:
- **libcosmic** (git, HEAD) — COSMIC UI framework (iced fork)
- **melib 0.8.13** — Mail engine from the meli project (IMAP, MIME parsing, envelope handling)

Target server: Runbox (mail.runbox.com:993, implicit TLS). Should work with any standard IMAP server.

## Critical: Version Pinning

melib 0.8.13's `imap` feature depends on `imap-codec` and `imap-types`. Newer alpha versions of these crates introduced a breaking change (missing `modifiers` field) that prevents compilation.

**The lockfile pins these to working versions:**
- `imap-codec = 2.0.0-alpha.4`
- `imap-types = 2.0.0-alpha.4`

**DO NOT run `cargo update` without verifying these pins are preserved.** If they drift, re-pin with:
```bash
cargo update -p imap-codec --precise 2.0.0-alpha.4
cargo update -p imap-types --precise 2.0.0-alpha.4
```

This is an upstream melib bug. Monitor melib releases for a fix.

## Architecture

This is a Cargo workspace with two crates:

- **nevermail-core** — Headless email engine (zero COSMIC deps). Library crate.
- **nevermail** (root) — COSMIC desktop GUI. Binary crate, depends on nevermail-core.

```
nevermail/                          (workspace root)
├── Cargo.toml                      (workspace + GUI binary package)
├── Cargo.lock                      (shared lockfile)
├── nevermail-core/
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

**Import conventions:** GUI code imports from `nevermail_core::` (config, imap, models, store, etc.) and `crate::` (dnd_models, app, ui). Core re-exports key melib types (`EnvelopeHash`, `MailboxHash`, `FlagOp`, `Flag`, `BackendEvent`, `RefreshEventKind`) so the GUI never depends on melib directly.

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

### MIME Body Extraction
Walks the attachment tree recursively looking for text/plain and text/html parts. Uses `Attachment::decode(Default::default())` for content-transfer-encoding. Prefers text/plain; falls back to html2text on text/html.

## Credentials (Phase 0)

Environment variables, no UI prompt:
```bash
export NEVERMAIL_SERVER=mail.runbox.com
export NEVERMAIL_PORT=993        # optional, default 993
export NEVERMAIL_USER=you@runbox.com
export NEVERMAIL_PASSWORD=yourpassword
export NEVERMAIL_STARTTLS=false  # optional, default false (implicit TLS)
```

Config::from_env() panics with a helpful message if required vars are missing.

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

## Known Limitations

- **No offline compose** — requires active IMAP session for SMTP relay config
- **Move revert on failure** — trash/archive failures don't re-insert the optimistically removed message; a manual refresh restores state

