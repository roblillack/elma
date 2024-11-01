# ELMA – Electronic mail agent

## TODO

- [ ] Enable mouse support
- [ ] Jump to next/previous message in message view
- [ ] Support for `multipart/alternative` and `multipart/related`
- [ ] Support for `application/ics` (iCalendar) to at least show basic content
- [ ] Support identifying and working with attachments

## User Manual

### Configuration

The configuration file is located at `~/.elmarc` or `~/.config/elma/config.toml`. The following is an example configuration file:

```toml
[gmail]
email = "my_user_name@gmail.com"
password = "MYAPPPASSWORD"
```

### Command line options

- `-D` \
  Enable demo mode. In demo mode, the program will not connect to any mail server, but instead use a set of predefined messages for demonstration purposes.

### Message flags

Messages are usually marked with flags to indicate their status. The following flags are supported:

- `A` ("Archived") \
  This message is marked for archival. Upon commit, the message will be moved to the archive folder.
- `D` ("Deleted") \
  This message is marked for deletion. Upon commit, the message will be moved to the trash folder.
- `N` ("New") \
  This message is new and has not been read yet.
- `*` ("Starred") \
  This message is starred.
- `→` ("Forwarded", alternatively `f`) \
  This message has been forwarded to you.
- `↩` ("Replied-to", alternatively `r`) \
  This message has been replied to already.
- `⇄` ("Forwarded and answered", alternatively `R`) \
  This message has been forwarded to you and answered.

### Keybindings

#### Message index

- `Up`/`Down` Move up/down (TODO: add `j`/`k`)
- `Enter`/`Right` View message

- `d` Mark message for deletion (alternatives: `Delete`/`Backspace`)
- `y` Mark message for archival
- `!` TODO: Mark message as spam
- `u` Undo planned changes or Mark unread
- `$` Commit planned changes
- `r` Reply
- `a` TODO: Reply all
- `f` TODO: Forward
- `x` TODO: Multi-select? and `*` to select all?

### Integration with Email servers/providers

Currently, ELMA supports connecting to Gmail accounts. To connect to a Gmail account, you need to create an "App password" in your Google account settings. This password is then used in the configuration file.

#### Folders and labels

TODO.

ELMA transparently handles the following folders for all supported email backends, regardless of the actual folder names used by the backend or if the backend uses labels instead of folders:

- `Inbox`: The inbox folder/label.
- `Archive`: Place for archived messages. In case an IMAP account is used which does not support labels, the messages are moved to the “Archive” folder.
- `Trash`: The trash folder/label.
- `Spam`: The spam folder/label.
- `Sent`: The sent folder/label.
- `Drafts`: The drafts folder/label.

#### Ignored IMAP labels

ELMA ignores the IMAP label `Important` which is used by Gmail to mark messages as important.
