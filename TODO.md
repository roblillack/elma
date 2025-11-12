# Elma TODO

Random things to do / implement / fix. Please don't regard this as an official
roadmap, just a brain dump of things that could be done, or that I thought of
while working on Elma.

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
- [ ] Implement `j` and `k` to move to next/previous message in mailbox view
- [x] Implement `j` and `k` to move to next/previous message in message view
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
- [x] Load mailbox while already showing it
- [ ] Only start connecting to backends when the user first tries to access them
- [ ] Backends should be able to express if they have support for "Important" flags & virtual folders
- [ ] Backends should be able to express if they have support for "Archive" vs "All Mail" folders (Gmail, sadly, does not have a true Archive folder)
- [x] Enable mouse support
- [x] Jump to next/previous message in message view
- [ ] Support for `multipart/alternative` and `multipart/related`
- [ ] Support for `application/ics` (iCalendar) to at least show basic content
- [ ] Support identifying and working with attachments
- [ ] `/` should search for messages in the current mailbox
- [ ] `%` (needs to be checked against Gmail web, fastmail web, Mutt) shulf offer a menu to FILTER the current mailbox against (to be selected by user) all mails with same list-id/sender/subject/date
- [ ] `!` should mark as Spam
- [ ] `.` Should open a MENU in most cases to allow the user to select the default and additional actions
- [ ] `?` Should open the online help whenever possible. This should be a full manual, not just a simple list of keybindings, because: The "default" actions are shown in the action bar anyhow, the additional commands are listed in the "context menu"
- [ ] Right-clicking should also open the context menu
- [ ] We should try out centered modal dialogs, instead of a command prompt at the bottom
- [ ] "toolbar buttons" should flash before the action is executed

## Other stuff

- We need commands to:
  - [ ] Open the current view (message or folder) in a web view (specific to the used backend) (`o`?)
  - [ ] View this message in a browser for selected extensions and/or use xdg-open to open the current message part using the default application
- Nice to have:
  - Build website/tool to compare html2txt/links/lynx/elinks vs. FTML import for random websites
- We should also look at these tools:
  - Newsboat: https://github.com/newsboat/newsboat
