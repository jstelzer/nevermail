# You want to use this with Gmail.

You think you do, but you don't.

Here's what it entails.

---

## Hoop 1: Google killed password login

You can't just type your Gmail password into an IMAP client anymore. Google
disabled "Less Secure Apps" access in May 2022. Your actual password will get
rejected with a cryptic auth failure.

You need an **App Password** — a 16-character generated token that bypasses
2FA for a single app.

To get one:
1. Enable 2-Step Verification on your Google account (you can't generate app
   passwords without it)
2. Go to https://myaccount.google.com/apppasswords
3. Pick a name ("Neverlight Mail" or whatever), generate
4. Google shows you a 16-character code **once**. Copy it. You will never see
   it again.

Use that as your password, not your real one.

If you're on a Google Workspace account, your admin may have disabled app
passwords entirely. In that case: you're done. Stop here. Use the web client.

## Hoop 2: Enable IMAP

Gmail has IMAP disabled by default.

1. Gmail → Settings (gear icon) → See all settings → Forwarding and POP/IMAP
2. Under "IMAP access", select "Enable IMAP"
3. Save

If you skip this, the connection will just hang or give you a generic "login
failed" — nothing useful.

## Hoop 3: Gmail's folder names are weird

Gmail doesn't really have folders. It has labels. IMAP access maps labels to
virtual folders, but the names are unusual:

| What you want      | What Gmail calls it       |
|---------------------|---------------------------|
| Sent                | `[Gmail]/Sent Mail`       |
| Trash               | `[Gmail]/Trash`           |
| Drafts              | `[Gmail]/Drafts`          |
| Spam                | `[Gmail]/Spam`            |
| Starred             | `[Gmail]/Starred`         |
| All Mail            | `[Gmail]/All Mail`        |
| Archive             | `[Gmail]/All Mail` (yes, same thing) |

Neverlight Mail finds Trash by looking for a folder with `\Trash` in the name or
path. Gmail's `[Gmail]/Trash` should match. Sent/Drafts might not get special
treatment — they'll just appear as regular folders in the sidebar.

The `/` separator in `[Gmail]/Sent Mail` is also a path separator, so nested
label hierarchies (like `Work/Projects/Active`) show up as deeply nested
folder trees.

## Hoop 4: "All Mail" is a trap

Gmail exposes `[Gmail]/All Mail`, which is every message that hasn't been
permanently deleted. If you select it, Neverlight Mail will try to sync every
message in your account. If you've had Gmail since 2004, that's potentially
hundreds of thousands of envelopes.

The initial fetch will take a very long time, eat memory, and possibly time
out. There's no pagination on the IMAP fetch — melib streams batches, but
it's still all of them.

Don't open All Mail. Just don't.

## Hoop 5: The SMTP server is different

Gmail's IMAP server is `imap.gmail.com:993`. But the SMTP server is
`smtp.gmail.com` on port `587` (STARTTLS).

Neverlight Mail derives SMTP config from your IMAP config — it reuses the same
server hostname by default. For Gmail, you need to override:

```bash
export NEVERLIGHT_MAIL_SMTP_SERVER=smtp.gmail.com
export NEVERLIGHT_MAIL_SMTP_PORT=587
```

Or in `~/.config/neverlight-mail/config.json`, the SMTP settings aren't persisted
yet — you'll need the env vars. The app password works for both IMAP and SMTP,
at least.

## Hoop 6: Gmail may throttle or block you

Google rate-limits IMAP connections. If you reconnect too aggressively (which
can happen during development, or if the IDLE connection drops and
reconnects), Google may temporarily lock you out with:

> `[ALERT] Application-specific password required`

or

> `Too many simultaneous connections`

There's no fix except waiting. Google's lockout lasts anywhere from a few
minutes to an hour.

## Hoop 7: Delete doesn't mean delete

In most IMAP servers, moving a message to Trash is straightforward. In Gmail,
deleting a message from any label just removes the label. To actually trash a
message, you have to move it to `[Gmail]/Trash`. Neverlight Mail does this — but if
Gmail's IMAP layer decides to interpret the operation as "remove label" instead
of "move to trash", the message just vanishes from the current view and
reappears in All Mail. Confusing.

## Hoop 8: Archive means "remove from Inbox"

On most IMAP servers, archive means "move to Archive folder." On Gmail,
archiving means "remove the Inbox label." The message stays in All Mail. If
you're in Neverlight Mail and you archive something, it'll disappear from Inbox but
won't show up in a dedicated Archive folder — because Gmail doesn't have one.
It's just All Mail.

---

## The actual config

If you've survived the above:

**Setup dialog / config file:**
- Server: `imap.gmail.com`
- Port: `993`
- Username: `you@gmail.com`
- Password: your 16-character app password
- STARTTLS: **off** (Gmail uses implicit TLS on 993)

**Environment overrides for SMTP:**
```bash
export NEVERLIGHT_MAIL_SMTP_SERVER=smtp.gmail.com
export NEVERLIGHT_MAIL_SMTP_PORT=587
```

**Or the full env-var route:**
```bash
export NEVERLIGHT_MAIL_SERVER=imap.gmail.com
export NEVERLIGHT_MAIL_PORT=993
export NEVERLIGHT_MAIL_USER=you@gmail.com
export NEVERLIGHT_MAIL_PASSWORD=abcd-efgh-ijkl-mnop
export NEVERLIGHT_MAIL_STARTTLS=false
export NEVERLIGHT_MAIL_SMTP_SERVER=smtp.gmail.com
export NEVERLIGHT_MAIL_SMTP_PORT=587
```

---

## What's temporary vs permanent

Some of this pain is Gmail being Gmail — that never gets better:
- App passwords (hoop 1)
- Label-as-folder mapping (hoops 3, 7, 8)
- All Mail footgun (hoop 4)
- Rate limiting (hoop 6)

Some of it is Neverlight Mail not having the plumbing yet — that gets better:
- **Split IMAP/SMTP config** is on the roadmap. Once it lands, the SMTP
  override env vars go away and you just fill in two servers in the setup
  dialog like a normal person.
- **Multi-account support** is on the roadmap. When that ships, you can
  run Gmail alongside a sane provider without juggling env vars or config
  files.

The goal isn't to make Gmail a first-class experience — it's to make
Neverlight Mail flexible enough that Gmail falls out as a side effect of doing
standard IMAP properly.

## On providers

Gmail works over IMAP because Google is required to provide the exit door.
That doesn't make it a good IMAP experience. The label system is a
compatibility shim, and every IMAP client fights it.

If you're choosing a provider and IMAP matters to you:
[Runbox](https://runbox.com),
[Fastmail](https://fastmail.com),
[Migadu](https://migadu.com),
[Posteo](https://posteo.de).

Real folders. Real IMAP. No app passwords. No label theatre. No
surveillance pricing model where you're the product.
