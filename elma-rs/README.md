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

The mock backend remains available via `--demo`. Without that flag the application looks for a
Gmail configuration in `~/.elmarc` and connects through Gmail's IMAP endpoint:

```toml
[gmail]
email = "user@gmail.com"
password = "app-specific-password"
```

If the configuration file (or the `gmail` section) is missing, the client falls back to the mock
backend so you can continue exploring the UI without network access. New mock messages arrive every
few seconds to keep the inbox active.

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
  - `d` Drafts
  - `t` Sent
  - `S` Spam (Should be `j` or even `!`?)
  - `T` Trash (Should be `g` `r` or even `g` `#`?)

### Todo

- [x] Add `!` to report as spam
- [x] Add `g` `S` (Should be `g` `j` or even `g` `!`) to go to spam mailbox
- [ ] Implement `r` to reply to message
- [ ] Implement `a` to reply all
- [ ] Implement `f` to forward message
- [ ] `=`/`+` and `-` to mark as important/unimportant
- [ ] Implement search (`/` and `n`/`N`)
- [ ] Implement composing new message (`c`)
- [ ] Implement help overlay (`?`)
- [ ] Implement `j` and `k` to move to next/previous message in mailbox view
- [ ] Implement `.` to open context menu for selected message

## Project layout

- `src/backend`: backend trait and the mock implementation.
- `src/ui`: Ratatui widgets and layout code.
- `src/viewer.rs`: FTML rendering helpers backed by `tdoc`.
