package backend

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"log"
	"os"
	"os/exec"
	"regexp"
	"strconv"
	"sync"
	"syscall"
	"time"

	imap "github.com/emersion/go-imap"
	compress "github.com/emersion/go-imap-compress"
	"github.com/emersion/go-imap/client"
	message "github.com/emersion/go-message"
	tcell "github.com/gdamore/tcell/v2"
	"github.com/rivo/tview"

	// Necessary to import "most common charsets" for go-message
	_ "github.com/emersion/go-message/charset"

	"github.com/roblillack/elma/events"
	"github.com/roblillack/elma/models"
)

// https://developers.google.com/gmail/imap/imap-smtp
type GmailBackend struct {
	Email    string
	Password string
	Client   *client.Client

	// updates    chan client.Update
	// eventQueue chan events.Event
	stopIdling chan struct{}
	idleMutex  sync.Mutex
	// inbox      map[models.MessageID]*models.Message

	archiveName string
	trashName   string
}

const GmailMessageID imap.FetchItem = `X-GM-MSGID`
const GmailLabelsAttribute imap.FetchItem = `X-GM-LABELS`
const defaultArchiveLabelName string = "[Gmail]/All Mail"
const defaultTrashLabelName string = "[Gmail]/Trash"

func getMessageLabels(msg *imap.Message) ([]string, bool) {
	raw, haveMsgID := msg.Items[GmailLabelsAttribute]
	if !haveMsgID {
		return nil, false
	}

	l, err := imap.ParseStringList(raw)
	return l, err == nil
}

func getMessageID(msg *imap.Message) (models.MessageID, bool) {
	raw, haveMsgID := msg.Items[GmailMessageID]
	if !haveMsgID {
		return 0, false
	}

	str, isStr := raw.(string)
	if !isStr {
		return 0, false
	}

	u, err := strconv.ParseUint(str, 10, 64)
	return models.MessageID(u), err == nil
}

var _ Backend = &GmailBackend{}

// var _ events.EventPublisher = &GmailBackend{}

func NewGmailBackend(email string) *GmailBackend {
	return &GmailBackend{
		Email: email,
	}
}

func NewGmailBackendWithPassword(email, pw string) *GmailBackend {
	return &GmailBackend{
		Email:    email,
		Password: pw,
	}
}

func getFromKeyChain(service, username string) (string, error) {
	ErrNotFound := errors.New("Password not found")
	pwRe := regexp.MustCompile(`password:\s+(?:0x[A-Fa-f0-9]+\s+)?"(.+)"`)
	escapeCodeRegexp := regexp.MustCompile(`\\([0-3][0-7]{2})`)

	unescapeOne := func(code []byte) []byte {
		i, _ := strconv.ParseUint(string(code[1:]), 8, 8)
		return []byte{byte(i)}
	}

	unescape := func(raw string) string {
		if !escapeCodeRegexp.MatchString(raw) {
			return raw
		} else {
			return string(escapeCodeRegexp.ReplaceAllFunc([]byte(raw), unescapeOne))
		}
	}

	args := []string{"find-internet-password", "-s", service, "-a", username, "-g"}
	c := exec.Command("/usr/bin/security", args...)
	o, err := c.CombinedOutput()
	if err != nil {
		exitCode := -1
		if c.ProcessState != nil && c.ProcessState.Sys() != nil {
			exitCode = c.ProcessState.Sys().(syscall.WaitStatus).ExitStatus()
		}
		// check particular exit code
		if exitCode == 44 {
			return "", ErrNotFound
		}
		return "", fmt.Errorf("/usr/bin/security: %s", err)
	}
	matches := pwRe.FindStringSubmatch(string(o))
	if len(matches) != 2 {
		return "", ErrNotFound
	}
	return unescape(matches[1]), nil
}

// func authenticate(c *client.Client, cfg *oauth2.Config, username string) error {
// 	if ok, err := c.SupportAuth(sasl.auth2); err != nil {
// 		return err
// 	} else if !ok {
// 		return errors.New("XOAUTH2 not supported by the server")
// 	}

// 	// Ask for the user to login with his Google account
// 	code, err := oauthdialog.Open(cfg)
// 	if err != nil {
// 		return err
// 	}

// 	// Get a token from the returned code
// 	// This token can be saved in a secure store to be reused later
// 	token, err := cfg.Exchange(oauth2.NoContext, code)
// 	if err != nil {
// 		return err
// 	}

// 	// Login to the IMAP server with XOAUTH2
// 	saslClient := sasl.NewXoauth2Client(username, token.AccessToken)
// 	return c.Authenticate(saslClient)
// }

func (b *GmailBackend) Initialize() error {
	if b.Password != "" {
		return nil
	}

	pw, err := getFromKeyChain("www.google.com", b.Email)
	if err != nil {
		log.Println(err)
		ok := false
		app := tview.NewApplication()
		inp := tview.NewInputField().SetLabel("Password: ").SetMaskCharacter('*').SetDoneFunc(func(key tcell.Key) {
			if key == tcell.KeyEnter {
				ok = true
				app.Stop()
			}
		})
		if err := app.SetRoot(inp, true).Run(); err != nil {
			return err
		}
		if !ok {
			return fmt.Errorf("Bah, no password entered.")
		}
		pw = inp.GetText()
	}
	b.Password = pw

	return nil
}

func (b *GmailBackend) Open() error {
	c, err := client.DialTLS("imap.gmail.com:993", nil)
	if err != nil {
		return err
	}

	// if ok, _ := c.Support("ENABLE"); !ok {

	// }
	// if _, err := enable.NewClient(c).SupportEnable(); err != nil {
	// 	return err
	// }

	comp := compress.NewClient(c)
	if ok, err := comp.SupportCompress(compress.Deflate); err != nil {
		return err
	} else if ok {
		if err := comp.Compress(compress.Deflate); err != nil {
			return err
		}

		log.Printf("Compression enabled: %t", comp.IsCompress())
	}

	if err := c.Login(b.Email, b.Password); err != nil {
		return err
	}

	// authenticate(c, cfg, b.Email)

	b.Client = c
	// b.Client.Updates = b.updates

	if ok, err := b.Client.Support("MOVE"); err != nil {
		return err
	} else {
		log.Printf("MOVE extension supported: %v\n", ok)
	}

	if err := b.determineSpecialFolders(); err != nil {
		return err
	}

	log.Println("Connection open.")

	return nil
}

func (b *GmailBackend) determineSpecialFolders() error {
	b.archiveName = defaultArchiveLabelName
	b.trashName = defaultTrashLabelName

	all := []*imap.MailboxInfo{}
	ch := make(chan *imap.MailboxInfo)

	go func() {
		for i := range ch {
			all = append(all, i)
		}
	}()

	if err := b.Client.List("", "*", ch); err != nil {
		return err
	}

	for _, folder := range all {
		for _, attr := range folder.Attributes {
			if attr == `\All` {
				b.archiveName = folder.Name
			} else if attr == `\Trash` {
				b.trashName = folder.Name
			}
		}
	}

	return nil
}

func (b *GmailBackend) Close() error {
	if b.stopIdling != nil {
		close(b.stopIdling)
		b.stopIdling = nil
	}
	if b.Client != nil {
		return b.Client.Logout()
	}

	return nil
}

func (b *GmailBackend) checkConnection() error {
	if b.Client == nil {
		return b.Open()
	}

	return nil
}

func (b *GmailBackend) selectMailbox(name string) (*imap.MailboxStatus, error) {
	if err := b.checkConnection(); err != nil {
		return nil, err
	}

	if s := b.Client.Mailbox(); s != nil && s.Name == name {
		return s, nil
	}

	st, err := b.Client.Select(name, false)
	if err != nil {
		return nil, fmt.Errorf("unable to select mailbox: %w", err)
	}

	return st, nil
}

func flagsContains(flags []string, what string) bool {
	for _, i := range flags {
		if i == what {
			return true
		}
	}
	return false
}

func isStarred(flags []string) bool {
	return flagsContains(flags, `\Flagged`)
}

func isAnswered(flags []string) bool {
	return flagsContains(flags, `\Answered`)
}

func isUnread(flags []string) bool {
	return !flagsContains(flags, `\Seen`)
}

func (b *GmailBackend) LoadInbox() ([]*models.Message, chan events.Event, error) {
	log.Println("GmailBackend.LoadInbox")
	// idle := b.pauseIdle()

	mbox, err := b.selectMailbox("INBOX")
	if err != nil {
		return nil, nil, err
	}
	seqset := &imap.SeqSet{Set: []imap.Seq{{Start: uint32(1), Stop: mbox.Messages}}}

	messages := make(chan *imap.Message, 100000)
	done := make(chan error)
	go func() {
		done <- b.Client.Fetch(seqset, []imap.FetchItem{GmailMessageID, GmailLabelsAttribute, imap.FetchFlags, imap.FetchInternalDate, imap.FetchRFC822Size, imap.FetchEnvelope, imap.FetchUid}, messages)
	}()

	// b.inbox = map[models.MessageID]*models.Message{}
	list := []*models.Message{}
	seqIDs := map[uint32]*models.Message{}
	for i := range messages {
		msg := b.buildMessage(i)
		if msg == nil {
			continue
		}
		// b.inbox[msg.ID] = msg
		seqIDs[msg.SequenceID] = msg
		list = append(list, msg)
	}

	if err := <-done; err != nil {
		return nil, nil, fmt.Errorf("unable to fetch messages: %s", err)
	}

	updates := make(chan client.Update)
	eventQueue := make(chan events.Event, 1000)

	go func() {
		f, err := os.OpenFile("events.log", os.O_APPEND|os.O_CREATE|os.O_RDWR, 0666)
		if err != nil {
			fmt.Printf("error opening file: %v", err)
		}
		defer f.Close()

		eventsLog := log.New(f, "", log.LstdFlags)

		for {
			update := <-updates
			switch u := update.(type) {
			case *client.StatusUpdate:
				eventsLog.Printf("Status update: %+v\n", u.Status)
			case *client.MailboxUpdate:
				eventsLog.Printf("Mailbox update: %+v\n", u.Mailbox)
				if newLen := int(u.Mailbox.Messages) - len(seqIDs); newLen > 0 {
					eventsLog.Printf("-> %d new messages\n", newLen)
					newMsgs, err := b.loadMessages(&imap.SeqSet{Set: []imap.Seq{{
						Start: uint32(len(seqIDs) + 1),
						Stop:  uint32(len(seqIDs) + newLen)}}})
					if err != nil {
						eventsLog.Printf("Unable to load missing messages: %s\n", err)
					}
					eventsLog.Printf("   loaded %d messages\n", len(newMsgs))
					for _, i := range newMsgs {
						seqIDs[i.SequenceID] = i
						list = append(list, i)
						eventQueue <- events.NewMessage{Message: i}
					}
				}
			case *client.MessageUpdate:
				eventsLog.Printf("Message update: %+v\n", u.Message)
				msg := seqIDs[u.Message.SeqNum]
				if msg != nil && flagsChanged(msg, u.Message) {
					updateFlags(msg, u.Message)
					eventQueue <- events.MessageFlagsChanged{Message: msg}
				}
			case *client.ExpungeUpdate:
				eventsLog.Printf("Expunge update: %+v\n", u.SeqNum)
				msg := seqIDs[u.SeqNum]
				if msg == nil {
					continue
				}

				newList := make([]*models.Message, 0, len(list)-1)
				seqIDs = map[uint32]*models.Message{}
				for _, i := range list {
					if i.ID == msg.ID {
						continue
					}
					if i.SequenceID > u.SeqNum {
						i.SequenceID--
					}
					seqIDs[i.SequenceID] = i
					newList = append(newList, i)
				}
				eventsLog.Printf("list %d --> %d\n", len(list), len(newList))
				eventsLog.Printf("seqIDs %d\n", len(seqIDs))
				list = newList
				eventQueue <- events.MessageDeleted{Message: msg}
			}
		}
	}()

	b.Client.Updates = updates
	b.ResumeEvents()

	return list, eventQueue, err
}

func getMessageParts(e *message.Entity, msg *models.Message, level int) ([]models.MessageContentPart, error) {
	if level >= 3 {
		return nil, errors.New("Too many levels of nesting")
	}

	if r := e.MultipartReader(); r != nil {
		// This is a multipart message
		list := []models.MessageContentPart{}
		for {
			p, err := r.NextPart()
			if errors.Is(err, io.EOF) {
				break
			} else if err != nil {
				log.Printf("Unable to read part of multipart message %v from %s: %s", msg.ID, msg.Sent.Format(time.RFC3339), err)
				break
			}

			parts, err := getMessageParts(p, msg, level+1)
			if err != nil {
				return nil, err
			}
			list = append(list, parts...)
		}
		return list, nil
	}

	t, _, _ := e.Header.ContentType()

	rawContent, err := io.ReadAll(e.Body)
	if err != nil {
		return nil, err
	}

	return []models.MessageContentPart{{ContentType: t, Content: rawContent}}, nil
}

func (b *GmailBackend) LoadMessageContent(m *models.Message) (*models.MessageContent, error) {
	log.Println("GmailBackend.LoadMessageContent")

	idle := b.pauseIdle()

	log.Println("No longer IDLEing ...")

	messages := make(chan *imap.Message, 1)
	done := make(chan error, 1)
	go func() {
		log.Println("Fetching message content ...")
		done <- b.Client.UidFetch(&imap.SeqSet{Set: []imap.Seq{{Start: m.UID, Stop: m.UID}}},
			[]imap.FetchItem{imap.FetchRFC822, imap.FetchBodyStructure}, messages)
	}()

	log.Println("Waiting for message content ...")
	msg := <-messages

	log.Println("Done waiting for message content ...")

	if err := <-done; err != nil {
		log.Fatal(err)
	}

	// ch := make(chan *imap.Message)
	// if err := b.Client.UidFetch(&imap.SeqSet{Set: []imap.Seq{{Start: m.UID, Stop: m.UID}}},
	// 	[]imap.FetchItem{imap.FetchFull}, ch); err != nil {
	// 	return nil, err
	// }

	if idle {
		log.Println("Resuming IDLE ...")
		b.ResumeEvents()
	}

	// log.Printf("ENVELOPE: %+v\n", msg.BodyStructure.Envelope)
	log.Printf("Message %s\nFrom %s\nSubject: %s\nDate: %s\n", m.ID, m.Sender, m.Subject, m.Sent.Format(time.RFC3339))
	for idx, part := range msg.BodyStructure.Parts {
		fn, _ := part.Filename()
		log.Printf("- ID: %s\n", part.Id)
		log.Printf("  PART: %d (description: %s, filename: %s)\n", idx+1, part.Description, fn)
		log.Printf("  TYPE: %s/%s\n", part.MIMEType, part.MIMESubType)
		log.Printf("  SIZE: %d\n", part.Size)
		log.Printf("  PARAMS: %+v\n", part.Params)
		log.Printf("  DISPOSITION: %+v\n", part.Disposition)
		log.Printf("  LANG: %s\n", part.Language)
		log.Printf("  LOCATION: %s\n", part.Location)
		log.Printf("  MD5: %s\n", part.MD5)
		log.Printf("  ENCODING: %s\n", part.Encoding)
	}
	sec, err := imap.ParseBodySectionName(imap.FetchRFC822)
	if err != nil {
		return nil, err
	}

	content, err := message.Read(msg.GetBody(sec))
	if message.IsUnknownCharset(err) || message.IsUnknownEncoding(err) {
		// This error is not fatal
		log.Printf("Unable to read message %v from %s: %s", m.ID, m.Sent.Format(time.RFC3339), err)
	} else if err != nil {
		log.Fatalf("Unable to read message %v from %s: %s", m.ID, m.Sent.Format(time.RFC3339), err)
	}

	parts, err := getMessageParts(content, m, 0)
	if err != nil {
		return nil, err
	}

	return &models.MessageContent{
		Mailer: content.Header.Get("X-Mailer"),
		Parts:  parts,
	}, nil
}

func (b *GmailBackend) loadMessages(seqSet *imap.SeqSet) ([]*models.Message, error) {
	idle := b.pauseIdle()

	messages := make(chan *imap.Message, 10000)
	done := make(chan error)
	go func() {
		done <- b.Client.Fetch(seqSet, []imap.FetchItem{GmailMessageID, GmailLabelsAttribute, imap.FetchFlags, imap.FetchInternalDate, imap.FetchRFC822Size, imap.FetchEnvelope, imap.FetchUid}, messages)
	}()

	list := []*models.Message{}
	for i := range messages {
		msg := b.buildMessage(i)
		if msg == nil {
			continue
		}
		list = append(list, msg)
	}

	if err := <-done; err != nil {
		return nil, fmt.Errorf("unable to fetch messages: %s", err)
	}

	if idle {
		b.ResumeEvents()
	}

	return list, nil
}

// func (b *GmailBackend) getInboxUpdates(stat *imap.MailboxStatus) ([]*models.Message, error) {
// 	seqset := &imap.SeqSet{Set: []imap.Seq{{Start: uint32(1), Stop: stat.Messages}}}

// 	messages := make(chan *imap.Message, 10000)
// 	done := make(chan error, 1)
// 	go func() {
// 		done <- b.Client.Fetch(seqset, []imap.FetchItem{GmailMessageID}, messages)
// 	}()

// 	fetchSet := &imap.SeqSet{}
// 	for i := range messages {
// 		if msgID, ok := getMessageID(i); ok {
// 			if _, known := b.inbox[msgID]; !known {
// 				fetchSet.AddNum(i.SeqNum)
// 			}
// 		}
// 	}

// 	if err := <-done; err != nil {
// 		return nil, fmt.Errorf("unable to fetch messages: %s", err)
// 	}

// 	list, err := b.loadMessages(fetchSet)
// 	if err != nil {
// 		return nil, err
// 	}

// 	for _, i := range list {
// 		b.inbox[i.ID] = i
// 	}

// 	return list, nil
// }

func flagsChanged(msg *models.Message, imapMsg *imap.Message) bool {
	st := models.StatusRead
	if isUnread(imapMsg.Flags) {
		st = models.StatusNew
	}
	if st != msg.Status {
		return true
	}

	return msg.Starred != isStarred(imapMsg.Flags) ||
		msg.Answered != isAnswered(imapMsg.Flags)
}

func updateFlags(msg *models.Message, imapMsg *imap.Message) {
	st := models.StatusRead
	if isUnread(imapMsg.Flags) {
		st = models.StatusNew
	}
	msg.Status = st
	msg.Starred = isStarred(imapMsg.Flags)
	msg.Answered = isAnswered(imapMsg.Flags)
}

func (b *GmailBackend) buildMessage(msg *imap.Message) *models.Message {
	if msg == nil {
		return nil
	}

	msgID, ok := getMessageID(msg)
	if !ok {
		return nil
	}

	st := models.StatusRead
	if isUnread(msg.Flags) {
		st = models.StatusNew
	}

	labels, _ := getMessageLabels(msg)
	return &models.Message{
		ID:         msgID,
		Sender:     msg.Envelope.From[0].PersonalName,
		Sent:       msg.Envelope.Date,
		Size:       int(msg.Size),
		Starred:    isStarred(msg.Flags),
		Answered:   isAnswered(msg.Flags),
		Status:     st,
		Subject:    msg.Envelope.Subject,
		Labels:     labels,
		SequenceID: msg.SeqNum,
		UID:        msg.Uid,
	}
}

func (b *GmailBackend) Subscribe() (<-chan events.Event, error) {
	// b.eventQueue = make(chan events.Event, 1000)

	// return b.eventQueue, b.resumeIdle()
	return make(chan events.Event), nil
}

func (b *GmailBackend) PauseEvents() {
	b.pauseIdle()
}

func (b *GmailBackend) pauseIdle() bool {
	if b.stopIdling != nil {
		log.Println("Pausing IDLE ....")
		close(b.stopIdling)
		b.idleMutex.Lock()
		b.stopIdling = nil
		b.idleMutex.Unlock()
		return true
	}

	return false
}

func (b *GmailBackend) ResumeEvents() {
	b.idleMutex.Lock()
	b.stopIdling = make(chan struct{})

	go func() {
		err := b.Client.Idle(b.stopIdling, nil)
		if err != nil {
			log.Printf("Error IDLEing: %s", err)
			// close(b.stopIdling)
			// b.stopIdling = nil
		}
		b.idleMutex.Unlock()
	}()

	log.Println("ok, we're IDLEing")
}

func (b *GmailBackend) handleServerUpdate(update client.Update) {
	switch u := update.(type) {
	// case *client.ExpungeUpdate:
	case *client.MessageUpdate:
		log.Printf("\n\nMESSAGE UPDATE\n%+v\n", u)
		// b.eventQueue <- events.NewMessage{Message: b.buildMessage(u.Message)}
	// case *client.MailboxUpdate:
	// 	if u.Mailbox.Name != "INBOX" {
	// 		return
	// 	}
	// 	newMsgs, err := b.getInboxUpdates(u.Mailbox)
	// 	if err != nil {
	// 		log.Printf("Error getting updates: %s\n", err)
	// 		return
	// 	}
	// 	for _, i := range newMsgs {
	// 		log.Printf("New: %s\n", i.Subject)
	// 		b.eventQueue <- events.NewMessage{Message: i}
	// 	}
	// 	b.eventQueue <- events.NewMessage{Message: b.buildMessage(u.Message)}
	default:
		log.Printf("Client update: %+v\n", update)
	}
	time.Sleep(3 * time.Second)
	// b.eventQueue <- update
	// return
}

func formatLabelList(list []string) (fields []interface{}) {
	fields = make([]interface{}, len(list))
	for i, v := range list {
		fields[i] = bytes.NewBufferString(v)
	}
	return
}

func (b *GmailBackend) DeleteMessage(msg *models.Message) error {
	idle := b.pauseIdle()
	err := b.Client.UidMove(&imap.SeqSet{Set: []imap.Seq{{Start: msg.UID, Stop: msg.UID}}}, b.trashName)
	if idle {
		b.ResumeEvents()
	}
	return err
}

func (b *GmailBackend) ArchiveMessage(msg *models.Message) error {
	// newLabels := []string{}
	// for _, i := range msg.Labels {
	// 	if i == b.archiveName {
	// 		continue
	// 	}
	// 	newLabels = append(newLabels, i)
	// }
	// newLabels = append(newLabels, b.archiveName)
	// log.Printf("\n\nlabels: %s --> %s\n\n\n", strings.Join(msg.Labels, ", "), strings.Join(newLabels, ", "))
	// time.Sleep(time.Second * 5)
	// return b.Client.Store(&imap.SeqSet{Set: []imap.Seq{{Start: msg.SequenceID, Stop: msg.SequenceID}}},
	// 	imap.StoreItem(GmailLabelsAttribute), formatStringList(newLabels), nil)
	idle := b.pauseIdle()
	err := b.Client.UidMove(&imap.SeqSet{Set: []imap.Seq{{Start: msg.UID, Stop: msg.UID}}}, b.archiveName)
	if idle {
		b.ResumeEvents()
	}
	return err

	crit := imap.NewSearchCriteria()
	err = crit.ParseWithCharset([]interface{}{
		string(GmailMessageID),
		fmt.Sprintf("%d", msg.ID),
	}, nil)
	if err != nil {
		return err
	}
	uids, err := b.Client.Search(crit)
	if err != nil {
		return err
	}
	if len(uids) > 1 {
		return fmt.Errorf("More than one UID returned for Message ID: %d", msg.ID)
	}
	if len(uids) < 1 {
		return fmt.Errorf("Message not found: %d", msg.ID)
	}
	log.Printf("Have seq ID: %d\n", uids[0])
	return b.Client.Store(&imap.SeqSet{Set: []imap.Seq{{Start: uids[0], Stop: uids[0]}}},
		imap.StoreItem(GmailLabelsAttribute),
		"\\Archive", nil)
}
