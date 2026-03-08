# Code Conventions

When in doubt, the Rust Book is canon: https://doc.rust-lang.org/book/

---

## State as Nouns, Not Adjective Combos

The single most important convention in this codebase. Model states as **named enum variants with embedded context**, not as combinations of boolean flags and Option fields.

### The problem

```rust
// BAD: state is spread across multiple fields
struct Connection {
    session: Option<Session>,
    is_connected: bool,
    is_syncing: bool,
    last_error: Option<String>,
    retry_count: u32,
}

// BAD: every call site interrogates the same booleans
if self.is_connected {
    if let Some(session) = &self.session {
        if !self.is_syncing {
            // finally do something
        }
    }
}
```

This creates impossible states (`is_connected = true` but `session = None`), duplicates decision logic across call sites, and buries the actual state machine in scattered `if let` trees.

### The fix

```rust
// GOOD: each state is a noun that carries its own context
enum ConnectionState {
    Disconnected,
    Connecting { domain: String },
    Connected(Session),
    Syncing { session: Session, since: State },
    Error { message: String, retries: u32 },
}
```

**Each variant is a named thing.** You can't be `Connected` without a `Session`. You can't be `Syncing` without a sync cursor. Invalid states are unrepresentable.

### Transitions are functions, not mutations

```rust
impl ConnectionState {
    fn on_event(self, event: Event) -> Self {
        match (self, event) {
            (Self::Connected(session), Event::SyncRequested) => {
                Self::Syncing {
                    since: session.last_state(),
                    session,
                }
            }
            (Self::Syncing { session, .. }, Event::SyncComplete(new_state)) => {
                Self::Connected(session.with_state(new_state))
            }
            (_, Event::Disconnected(reason)) => {
                Self::Error { message: reason, retries: 0 }
            }
            (state, _) => state,
        }
    }
}
```

Takes `self` by value, returns the next state. No mutation of Option fields. No boolean toggling. The compiler enforces that you handle every combination.

---

## `let-else` Over Nested `if let`

For linear "bail if this isn't what I expect" flows, use `let-else` (RFC 3137). This keeps the happy path at the left margin.

```rust
// BAD: nesting obscures the happy path
fn process(input: Option<&str>) -> Result<Output, Error> {
    if let Some(value) = input {
        if let Ok(parsed) = value.parse::<u64>() {
            if parsed > 0 {
                Ok(do_work(parsed))
            } else {
                Err(Error::InvalidInput("must be positive"))
            }
        } else {
            Err(Error::ParseFailed)
        }
    } else {
        Err(Error::MissingInput)
    }
}

// GOOD: flat, early returns, happy path is obvious
fn process(input: Option<&str>) -> Result<Output, Error> {
    let Some(value) = input else {
        return Err(Error::MissingInput);
    };
    let Ok(parsed) = value.parse::<u64>() else {
        return Err(Error::ParseFailed);
    };
    if parsed == 0 {
        return Err(Error::InvalidInput("must be positive"));
    }
    Ok(do_work(parsed))
}
```

References:
- https://doc.rust-lang.org/book/ch06-03-if-let.html
- https://github.com/rust-lang/rfcs/blob/master/text/3137-let-else.md

---

## Match Once at the Boundary

When you receive an enum, match it **once** at the entry point and dispatch to typed functions. Don't re-match or re-interrogate deeper in the call stack.

```rust
// GOOD: match once, delegate to typed handlers
fn handle_message(state: &mut AppState, msg: Message) -> Task {
    match msg {
        Message::FoldersLoaded(result) => handle_folders_loaded(state, result),
        Message::SyncComplete(event) => handle_sync_complete(state, event),
        Message::FlagOpResult(id, result) => handle_flag_result(state, id, result),
    }
}

// Each handler receives exactly the data it needs — no re-matching.
fn handle_folders_loaded(state: &mut AppState, result: Result<Vec<Folder>, JmapError>) -> Task {
    let Ok(folders) = result else {
        state.phase = Phase::Error;
        return Task::none();
    };
    state.folders = folders;
    state.phase = Phase::Idle;
    Task::none()
}
```

---

## Error Types

Use `thiserror` for error enums. Keep variants specific — not `Generic(String)` catch-alls.

```rust
#[derive(Debug, thiserror::Error)]
pub enum JmapError {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JMAP method error: {method} ({error_type})")]
    MethodError { method: String, error_type: String },

    #[error("full resync required")]
    CannotCalculateChanges,
}
```

At module boundaries where you cross from library errors to application errors, convert explicitly. Don't propagate `reqwest::Error` to the GUI — wrap it in a domain error.

---

## Naming

Follow Rust standard conventions (https://rust-lang.github.io/api-guidelines/naming.html):

- Types: `PascalCase` — `EmailId`, `SyncState`, `JmapClient`
- Functions/methods: `snake_case` — `fetch_messages`, `parse_body`
- Constants: `SCREAMING_SNAKE_CASE` — `DEFAULT_PAGE_SIZE`, `CAP_MAIL`
- Modules: `snake_case` — `client`, `session`, `store`

Domain-specific naming:
- Use JMAP terminology where it matches the RFC: `EmailId`, `MailboxId`, `BlobId`, `State`
- Use plain English where JMAP is jargon-heavy: `Folder` not `Mailbox` in the UI model, `MessageSummary` not `EmailGetResponse`
- The `types.rs` module bridges between these — JMAP-native types that are ergonomic to use

---

## Module Organization

Each module has a single responsibility. Split by **direction of data flow**, not by domain noun:

- `client.rs` — outward (HTTP transport, sending requests)
- `session.rs` — inward (parsing server capabilities)
- `email.rs` — JMAP Email methods (query, get, set, changes)
- `mailbox.rs` — JMAP Mailbox methods
- `sync.rs` — orchestration (combines email + mailbox changes into a sync loop)
- `parse.rs` — inward (RFC 5322 bytes → structured data)
- `store/` — downward (cache persistence)
- `models.rs` — shared data structures (protocol-neutral, used by UI)
- `types.rs` — shared JMAP types (protocol-specific, used by engine internals)

Don't create a module until it has a reason to exist. A function that's only called in one place can live in the caller's module.

---

## Warnings Are Errors

Treat compiler warnings and clippy lints as defects. Fix them, don't suppress them.

### No suppression without discussion

Do not add `#[allow(...)]`, `#[cfg_attr(..., allow(...))]`, or `#![allow(...)]` to silence warnings. If a lint fires, either fix the code or discuss why suppression is the right call. This includes:

- `#[allow(dead_code)]` — if it's dead, delete it.
- `#[allow(unused_variables)]` — prefix with `_` if intentionally unused, or remove the variable.
- `#[allow(clippy::...)]` — clippy is usually right. If it's wrong for a specific case, explain why in a comment next to the suppression.

The only pre-approved exception is `#[allow(clippy::type_complexity)]` on `CacheCmd` where the oneshot reply types make signatures long. Everything else requires explicit agreement.

### Clippy

Run `cargo clippy` before considering a piece of work done. Clippy catches real bugs (incorrect `clone`, needless borrows, redundant closures) and enforces idiomatic Rust. Treat it as a mentor, not a nuisance.

### Dead code

Delete it. Don't comment it out, don't gate it behind `#[cfg(never)]`, don't leave `// TODO: remove` notes. Git preserves history. If you need it back, find it in the log.

---

## One Function, One Truth

If logic appears in two places, extract it into a pure function. A **pure function** takes inputs, returns outputs, and has no side effects — easy to test, easy to reason about.

```rust
// BAD: flag encoding duplicated in two handlers
fn handle_toggle_read(msg: &MessageSummary) -> u8 {
    let mut f: u8 = 0;
    if !msg.is_read { f |= 1; }
    if msg.is_starred { f |= 2; }
    f
}

fn handle_toggle_star(msg: &MessageSummary) -> u8 {
    let mut f: u8 = 0;
    if msg.is_read { f |= 1; }
    if !msg.is_starred { f |= 2; }
    f
}

// GOOD: single pure function, callers compose
fn flags_to_u8(is_read: bool, is_starred: bool) -> u8 {
    let mut f: u8 = 0;
    if is_read { f |= 1; }
    if is_starred { f |= 2; }
    f
}
// Callers: flags_to_u8(!msg.is_read, msg.is_starred)
//          flags_to_u8(msg.is_read, !msg.is_starred)
```

Signs you need to extract:
- Two blocks of code that look similar but differ in one or two values.
- A function that takes a boolean parameter to switch between two behaviors — split into two callers of a shared pure function instead.
- Test setup code duplicated across test functions — extract a `sample_*()` helper.

The extracted function should be testable on its own. If it can't be tested without mocking half the system, it's not pure enough — keep decomposing.

---

## Tests

Tests are not optional. New logic gets tests. Changed logic gets updated tests. If a function is pure, it's trivially testable — that's the point.

- Unit tests live in `#[cfg(test)] mod tests` at the bottom of each module.
- Integration tests that need a real JMAP server live in `tests/` and gate on env vars (`NEVERLIGHT_MAIL_JMAP_TOKEN`, `NEVERLIGHT_MAIL_USER`).
- Each test function tests one behavior. Name it after what it asserts: `flags_from_empty_keywords`, `rejects_missing_mail_capability`, `folders_are_isolated_per_account`.
- Use `assert_eq!` with descriptive messages when the failure isn't obvious from the values alone.
- Test fixtures (real emails, JSON responses) go in `tests/fixtures/`.
- Extract `sample_*()` helpers for test data construction — don't duplicate struct literals across tests.
- Run `cargo test` before considering any change done. Run `cargo clippy` too.

---

## What Not to Do

- **No nested `if let` trees.** Use `let-else`, `match`, or early returns.
- **No boolean flags for state.** Use enums with context.
- **No `unwrap()` in library code.** Use `?`, `let-else`, or explicit error handling.
- **No `#[allow(...)]` without discussion.** Fix the warning or explain why suppression is correct.
- **No dead code.** Delete it. Git has history.
- **No duplicated logic.** Extract a pure function.
- **No speculative abstraction.** Don't add traits, generics, or configurability until there's a second consumer.
- **No protocol abstraction.** This is JMAP-only. Don't design for hypothetical IMAP support.
- **No `Clone` on large types just to avoid lifetime issues.** If you're cloning a `Vec<MessageSummary>` to avoid a borrow, rethink the data flow.
- **No untested code.** If it's worth writing, it's worth testing.
