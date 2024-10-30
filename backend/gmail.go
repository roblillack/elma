package backend

import (
	"bytes"
	"errors"
	"fmt"
	"log"
	"os"
	"os/exec"
	"reflect"
	"regexp"
	"strconv"
	"sync"
	"syscall"
	"time"

	imap "github.com/emersion/go-imap"
	compress "github.com/emersion/go-imap-compress"
	"github.com/emersion/go-imap/client"
	tcell "github.com/gdamore/tcell/v2"
	"github.com/rivo/tview"

	"github.com/roblillack/elma/events"
	"github.com/roblillack/elma/models"
)

const IdleSupport = true

// https://developers.google.com/gmail/imap/imap-smtp
type GmailBackend struct {
	Email    string
	Password string
	Client   *client.Client

	// updates    chan client.Update
	// eventQueue chan events.Event

	// used to signal the goroutine watching for updates to stop
	stopIdling chan struct{}
	// used to wait for the goroutine watching for updates to stop
	idleDone  chan struct{}
	idleMutex sync.Mutex
	// inbox      map[models.MessageID]*models.Message
	logfile *os.File

	archiveName string
	trashName   string

	// current folder
	folderLock  sync.Mutex
	folderName  string
	messageList []*models.Message
	msgsBySeqID map[uint32]*models.Message
	eventQueue  chan events.Event
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
var _ events.EventPublisher = &GmailBackend{}

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
	f, err := os.OpenFile("events.log", os.O_APPEND|os.O_CREATE|os.O_RDWR, 0666)
	if err != nil {
		fmt.Printf("error opening file: %v", err)
	}
	b.logfile = f

	log.SetOutput(b.logfile)

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
	b.Client.ErrorLog = log.Default()
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
	c := make(chan struct{})

	go func() {
		for i := range ch {
			all = append(all, i)
		}
		close(c)
	}()

	if err := b.Client.List("", "*", ch); err != nil {
		return err
	}
	<-c

	for _, folder := range all {
		for _, attr := range folder.Attributes {
			if attr == `\All` {
				b.archiveName = folder.Name
			} else if attr == `\Trash` {
				b.trashName = folder.Name
			}
		}
	}

	log.Printf("Trash folder: %s\n", b.trashName)
	log.Printf("Archive folder: %s\n", b.archiveName)

	return nil
}

func (b *GmailBackend) Close() error {
	if b.logfile != nil {
		b.logfile.Close()
	}
	if b.stopIdling != nil {
		close(b.stopIdling)
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

func (b *GmailBackend) idle() {
	if !IdleSupport {
		log.Println("Skipping IDLE.")
		return
	}

	log.Println("Preparing to idle...")
	b.idleMutex.Lock()

	// Create a channel to receive mailbox updates
	updates := make(chan client.Update)
	b.Client.Updates = updates
	b.stopIdling = make(chan struct{})
	// b.idleDone = make(chan struct{})

	done := make(chan error, 1)

	go func() {
		// Listen for updates
		for {
			log.Println("IDLE2: Waiting for updates...")
			select {
			case update := <-updates:
				log.Println("IDLE2: New update:", update)
				b.processUpdate(update)
				// if !stopped {
				// 	close(b.stopIdling)
				// 	stopped = true
				// }
			case err := <-done:
				if err != nil {
					log.Fatal(err)
				}
				log.Println("IDLE2: Not idling anymore")
				// close(b.idleDone)
				b.idleMutex.Unlock()
				return
			}
		}
	}()

	go func() {
		log.Println("IDLE1: Idling...")
		done <- b.Client.Idle(b.stopIdling, nil)
		log.Println("IDLE1: Done")
	}()
}

func (b *GmailBackend) processUpdate(update client.Update) {
	log.Println("Processing update:", update)

	log.Printf("Update: %s\n", reflect.TypeOf(update).String())
	switch u := update.(type) {
	case *client.StatusUpdate:
		log.Printf("Status update: %+v\n", u.Status)
	case *client.MailboxUpdate:
		log.Printf("Mailbox update: %+v\n", u.Mailbox)
	case *client.MessageUpdate:
		log.Printf("Message update: %+v\n", u.Message)
		msg := b.msgsBySeqID[u.Message.SeqNum]
		if msg != nil && flagsChanged(msg, u.Message) {
			updateFlags(msg, u.Message)
			b.eventQueue <- events.MessageFlagsChanged{Message: msg}
		}
	case *client.ExpungeUpdate:
		log.Printf("Expunge update: %+v\n", u.SeqNum)
		msg := b.msgsBySeqID[u.SeqNum]
		if msg == nil {
			return
		}

		b.folderLock.Lock()
		defer b.folderLock.Unlock()

		newList := make([]*models.Message, 0, len(b.messageList)-1)
		b.msgsBySeqID = map[uint32]*models.Message{}
		for _, i := range b.messageList {
			if i.ID == msg.ID {
				continue
			}
			if i.SequenceID > u.SeqNum {
				i.SequenceID--
			}
			b.msgsBySeqID[i.SequenceID] = i
			newList = append(newList, i)
		}
		log.Printf("list %d --> %d\n", len(b.messageList), len(newList))
		log.Printf("seqIDs %d\n", len(b.msgsBySeqID))
		b.messageList = newList
		b.eventQueue <- events.MessageDeleted{Message: msg}
	}
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
	done := make(chan error, 1)
	go func() {
		done <- b.Client.Fetch(seqset, []imap.FetchItem{GmailMessageID, GmailLabelsAttribute, imap.FetchFlags, imap.FetchInternalDate, imap.FetchRFC822Size, imap.FetchEnvelope, imap.FetchUid}, messages)
	}()

	b.folderLock.Lock()
	defer b.folderLock.Unlock()
	// b.inbox = map[models.MessageID]*models.Message{}
	b.folderName = "INBOX"
	b.messageList = []*models.Message{}
	b.msgsBySeqID = map[uint32]*models.Message{}
	b.eventQueue = make(chan events.Event)

	for i := range messages {
		msg := b.buildMessage(i)
		if msg == nil {
			continue
		}
		// b.inbox[msg.ID] = msg
		b.msgsBySeqID[msg.SequenceID] = msg
		b.messageList = append(b.messageList, msg)
	}

	if err := <-done; err != nil {
		return nil, nil, fmt.Errorf("unable to fetch messages: %s", err)
	}

	b.idle()

	return b.messageList, b.eventQueue, err
}

func (b *GmailBackend) loadMessages(seqSet *imap.SeqSet) ([]*models.Message, error) {
	messages := make(chan *imap.Message, 10000)
	done := make(chan error, 1)
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

func (b *GmailBackend) pauseIdle() bool {
	if b.stopIdling != nil {
		log.Println("Pausing IDLE ....")
		close(b.stopIdling)
		//<-b.idleDone
		b.idleMutex.Lock()
		log.Println("IDLE seems done....")

		b.idleMutex.Unlock()
		return true
	}

	return false
}

func (b *GmailBackend) resumeIdle() error {
	b.idle()
	return nil
	// log.Println("Resuming IDLE ....")
	// _, err := b.selectMailbox("INBOX")
	// if err != nil {
	// 	return err
	// }
	// log.Println("INBOX selected....")

	// b.stopIdling = make(chan struct{})
	// go func() {
	// 	err := b.Client.Idle(b.stopIdling, nil)
	// 	if err != nil {
	// 		log.Printf("Error IDLEing: %s", err)
	// 	}
	// }()

	// log.Println("ok, we're IDLEing")
	// return err
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

func (b *GmailBackend) Unsubscribe() error {
	if b.stopIdling == nil {
		return nil
	}

	b.stopIdling <- struct{}{}
	return nil
}

func formatLabelList(list []string) (fields []interface{}) {
	fields = make([]interface{}, len(list))
	for i, v := range list {
		fields[i] = bytes.NewBufferString(v)
	}
	return
}

func (b *GmailBackend) DeleteMessage(msg *models.Message) error {
	log.Printf("DeleteMessage: %d / %d / %d\n", msg.UID, msg.ID, msg.SequenceID)
	idle := b.pauseIdle()
	log.Println("DeleteMessage: triggering move")
	log.Println(b.Client.Check())
	err := b.Client.UidMove(&imap.SeqSet{Set: []imap.Seq{{Start: msg.UID, Stop: msg.UID}}}, b.trashName)
	log.Println(err)
	if idle {
		b.resumeIdle()
	}
	log.Println("DeleteMessage: resumed idle")
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
		b.resumeIdle()
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
