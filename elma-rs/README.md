# elma-rs

`elma-rs` is a Ratatui-based experimental port of the ELMA terminal mail agent. This prototype
focuses on re-creating the classic inbox and message views while relying on the
[`tdoc`](https://crates.io/crates/tdoc) toolkit to parse HTML into FTML documents for terminal
rendering.

## Features

- Ratatui inbox layout with action bar, tview-style message table, and matching colours.
- Full-screen message view with FTML formatting and message part summaries rendered by `tdoc`.
- Mock backend that continually injects fresh sample messages with realistic senders and dates.
- Keybindings mirror the Go implementation (star/archive/delete/commit, `j`/`k` navigation, etc.).

## Differences from the Go implementation

- Only the inbox view is implemented; modal dialogs, menus, and the multi-screen controller stack from the Go code are not yet ported.
- Message actions are simulated in-memory; archive/delete just update the local list and do not persist between runs.
- FTML rendering uses the `tdoc` crate directly rather than the Go `ftml` bindings, so styling may vary on edge cases.
- Help overlays, context menus (the Go `.` menu), and mouse interactions beyond basic selection are not implemented.
- Logging/debug output differs: the Rust version does not emit action/event logs compatible with `elma.log`.
- Theming and clipboard/web integration hooks from the Go app are currently missing.

## Running

```bash
cargo run -- --demo
```

The mock backend remains available via `--demo`. Without that flag the application loads accounts
from `~/.elmarc`. Multiple accounts are supported via the `[[accounts]]` table:

```toml
[[accounts]]
name = "Private"
backend = "gmail"
username = "user@gmail.com"
password = "app-specific-password"

[[accounts]]
name = "Demo"
backend = "demo"
```

`backend = "gmail"` expects an app password; `backend = "demo"` (or `"mock"`) uses the bundled
mock backend. For backwards compatibility, the legacy `[gmail]` section remains supported and is
treated as a single Gmail account. If no configuration is found the client falls back to the mock
backend so you can continue exploring the UI without network access. New mock messages arrive every
few seconds to keep the inbox active.

### General use

Elma presents a terminal-based email client interface with a focus on keyboard navigation and
efficiency. The main screen displays a list of emails in the selected mailbox, along with key
information such as sender, subject, and date. Users can navigate through emails, open them for
reading, and perform various actions like archiving, deleting, or starring messages.

### Acting on messages

Elma differentiates between two different types of actions on messages:

- Immediate actions: These simple actions are applied to the message right away. For example, starring or
  unstarring a message is an immediate action. Actions like these can usually easily be undone by
  performing the opposite action (e.g., unstarring a starred message) if you notice a mistake.

- Scheduled actions: These actions are marked for later application. For example, when you delete a
  message, it is marked for deletion but not removed from the list until you commit all the scheduled changes (by
  pressing `$`). This allows you to quickly go through a large amount of messages and review and modify your scheduled actions before they are finalized.

### Email flags

Message status is indicated by a four symbol character prefix in the message list:

1. Read/unread/scheduled action status:
   - ` `: This is a read message
   - `N`: New/unread message
   - `D`: Scheduled for deletion
   - `A`: Scheduled for archival
   - `!`: Scheduled to move to spam
   - `I`: Scheduled to move to inbox
2. Starred/unstarred and important status:
   - ` `: Regular message
   - `*`: Starred
   - `○`: Marked as important (ASCII mode: `+`)
   - `⊛`: Marked as important and starred (ASCII mode: `#`)
3. Reply/forward state:
   - ` `: No reply/forward
   - `↩`: This message has been replied to (ASCII mode: `r`)
   - `→`: This message has been forwarded (ASCII mode: `f`)
   - `⇄`: This message has been both replied to and forwarded (ASCII mode: `x`)
4. Attachment indicator: (TODO)
   - ` `: No attachment
   - `@`: Message has one or more attachments

### Key bindings

- `Ctrl+Q` quits to the shell.
- `Enter`/`Right` opens the selected message; `Esc`/`Left` closes the viewer.
- `d`, `Delete`, or `Backspace` schedule the message for deletion.
- `y` schedules the message for archival.
- `s` toggles the star flag; `u` toggles unread/read state.
- `$` commits scheduled actions (removing archived/deleted messages from the list).
- Arrow keys, `PageUp`/`PageDown`, `Home`, `End` move the cursor in the inbox.
- While a message is open, `j` / `k` jump to the next/previous message and `.` toggles raw HTML.
- `g` go to mailbox:
  - `i` Inbox
  - `a` Archive
  - `s` Starred
  - `I` Important
  - `d` Drafts
  - `t` Sent
  - `S` Spam (Should be `j` or even `!`?)
  - `T` Trash (Should be `g` `r` or even `g` `#`?)

### Todo

- [ ] Rename special use mailboxes to standard names (Junk, Flagged)?
- [ ] Implement transparent support of accounts which have Archive support vs. All Mail support?
- [x] Add `!` to report as spam
- [x] Add `g` `S` (Should be `g` `j` or even `g` `!`) to go to spam mailbox
- [ ] Implement `r` to reply to message
- [ ] Implement `a` to reply all
- [ ] Implement `f` to forward message
- [x] Add "Important" flag support
- [ ] Add attachment indicator to message list
- [x] `=`/`+` and `-` to mark as important/unimportant
- [ ] Implement search (`/` and `n`/`N`)
- [x] Implement composing new message (`c`)
- [ ] Implement help overlay (`?`)
- [x] Implement `j` and `k` to move to next/previous message in mailbox view
- [ ] Implement `.` to open context menu for selected message
- [x] Add support for multiple accounts/backends (`G` to Go to different account?)
- [x] In "Drafts" mailbox, show recipients in the message list
- [x] Allow continued editing of drafts (Enter to open draft in compose view again)
- [ ] Add auto-complete for email addresses when composing messages based on the user's contacts or previously sent emails and last 1000 messages
- [x] Add showing labels in the message view
- [ ] Show FTML-based preview in compose dialog -- for editing open the draft as Markdown in the user's $EDITOR
- [ ] Add support for attachments when composing messages
- [ ] Switch from hardcoded folder named in Gmail backend to standard "Special Use" folders via IMAP (See [RFC 6154](https://datatracker.ietf.org/doc/html/rfc6154)) as per [Google Developer Docs](https://developers.google.com/workspace/gmail/imap/imap-extensions#special-use_extension_of_the_list_command)
- [ ] Add command (`o`?) to open the current message in the web interface
- [ ] Add support to copy the current message's content to the clipboard as Markdown (context menu?)
- [ ] Add theming support (light/dark mode, custom colours)
- [x] Log network actions and user events for debugging
- [ ] Formatted date does not match web interface -- are we missing locale info somewhere?
- [ ] Load mailbox while already showing it
- [ ] Only start connecting to backends when the user first tries to access them
- [ ] Backends should be able to express if they have support for "Important" flags & virtual folders
- [ ] Backends should be able to express if they have support for "Archive" vs "All Mail" folders (Gmail, sadly, does not have a true Archive folder)

## Project layout

- `src/backend`: backend trait and the mock implementation.
- `src/ui`: Ratatui widgets and layout code.
- `src/viewer.rs`: FTML rendering helpers backed by `tdoc`.
