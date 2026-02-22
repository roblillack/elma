# Elma – Electronic Mail Agent

Elma is a Ratatui-based terminal mail user agent. It focuses on keyboard-driven
navigation and efficiency, inspired by tools like Mutt, Pine, and Elm, while
offering a modern feature set out-of-the-box, including:

- Crisp HTML email rendering,
- Server-side search, spam handling, and draft management,
- Support for special-use mailboxes and labels, like starring and marking as
  important,
- Multiple account support,
- Scheduled message actions (archive/delete),
- Extensibility for additional backends.

Elma strives to provide a productive and enjoyable email experience directly
from the terminal while staying compatible with the established workflows of
current web-based email services.

## Running

A “mock” backend is available via `--demo` for you to try out Elma without
setting up an email account:

```bash
cargo run -- --demo
```

Without that flag the application loads accounts from `~/.elmarc`. Multiple
accounts are supported via the `[[accounts]]` table:

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

Supported backends are:

- `gmail`: Connects to Gmail via IMAP and SMTP.
- `jmap`: Connects to any JMAP-compatible server (e.g., Fastmail).
- `demo`: A mock backend that generates fake messages for demonstration
  purposes.

If no configuration is found the client falls back to the mock backend so you
can continue exploring the UI without network access. New mock messages arrive
every few seconds to keep the inbox active.

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

  Scheduled actions (`d`, `y`, `!`) are **idempotent**: pressing the same key on
  a message that is already scheduled for that action is a no-op — it keeps the
  action and advances to the next message. This means you can sweep through a
  list pressing `d` repeatedly without accidentally undeleting a message you
  marked earlier. To undo a scheduled action, press `u` — this removes the
  pending action, restores the message's original status, and advances to the
  next message.

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
- `!` schedules the message to move to spam.
- `u` unschedules a pending action (delete/archive/spam/move-to-inbox), or toggles unread/read state on normal messages.
- `s` toggles the star flag.
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

When viewing a special mailbox (Archive, Spam, Trash), the primary action key for that mailbox is flipped: `d` in Trash, `y` in Archive, and `!` in Spam each schedule a move back to inbox instead. Pressing `u` then unstages that move, keeping the message where it is.
  
