# Mailbox Domain State Machine Contract (Client-Agnostic)

## Purpose

Define the canonical mailbox domain behavior independent of UI stack.

This contract should be implemented by:

- SwiftUI client domain store (`MailStore` + projection layer)
- Linux/TUI domain loop/store

## Ownership Model

- One serialized domain owner per process (actor or event loop).
- Canonical mailbox TOC/state lives only in domain owner.
- UI layers consume immutable snapshots and dispatch intents.

## Canonical Data Rules

1. IMAP TOC is canonical truth.
2. SQLite/cache TOC is provisional only.
3. If IMAP TOC and cache TOC differ, domain state must converge to IMAP TOC.
4. Selection must be revalidated against canonical TOC on every apply.

## Intents

- `selectMailbox(accountId, mailboxHash)`
- `selectMessage(messageId)`
- `refresh(force)`
- `search(query)`
- `toggleRead(messageId)`
- `toggleStar(messageId)`
- `move(messageId, destinationMailbox)`
- `delete(messageId)`

## Lanes

Lanes serialize intent classes and isolate cancellation/tokening.

- `folder`
- `message`
- `search`
- `refresh`
- `mutation`
- `flag`

Each lane guarantees:

- at most one active operation
- monotonic lane token/epoch
- stale apply is dropped when token mismatches
- newer intent cancels superseded in-flight work

## Required Postconditions

### Delete / Move

- After success, source mailbox TOC must not contain target message.
- If postcondition fails, domain must:
  - reconcile source TOC from IMAP
  - emit retryable error state

### Flag Changes (read/star)

- Canonical TOC entry is updated in domain state.
- Unread deltas never drive counts below zero.

## Snapshot Requirements

Every snapshot must include enough state for renderers to be stateless:

- accounts/folders projection
- selected mailbox + selected message projection
- projected TOC list
- phase (`idle/loading/refreshing/searching/error`)
- status/error surface suitable for user feedback

## Diagnostics (Recommended)

- lane operation IDs
- TOC drift count
- postcondition failure count
- refresh stuck/timeout counters

## Non-Goals

- UI widget behavior/details
- platform-specific entitlement/permission UX
- transport-specific details beyond canonical TOC policy
