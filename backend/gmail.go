package backend

import (
	"errors"
	"fmt"
	"log"
	"os/exec"
	"reflect"
	"regexp"
	"strconv"
	"syscall"

	"github.com/roblillack/elma/events"

	"github.com/emersion/go-imap"
	compress "github.com/emersion/go-imap-compress"
	"github.com/emersion/go-imap-enable"
	idle "github.com/emersion/go-imap-idle"
	"github.com/emersion/go-imap/client"
	oauthdialog "github.com/emersion/go-oauthdialog"
	sasl "github.com/emersion/go-sasl"
	"github.com/roblillack/elma/models"
	"golang.org/x/oauth2"
)

type GmailBackend struct {
	Email    string
	Password string
	Client   *client.Client

	eventQueue  chan events.Event
	idleChannel chan error
	inbox       map[models.MessageID]*models.Message
}

var GmailMessageID imap.FetchItem = `X-GM-MSGID`

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
		exitCode := c.ProcessState.Sys().(syscall.WaitStatus).ExitStatus()
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

func authenticate(c *client.Client, cfg *oauth2.Config, username string) error {
	if ok, err := c.SupportAuth(sasl.Xoauth2); err != nil {
		return err
	} else if !ok {
		return errors.New("XOAUTH2 not supported by the server")
	}

	// Ask for the user to login with his Google account
	code, err := oauthdialog.Open(cfg)
	if err != nil {
		return err
	}

	// Get a token from the returned code
	// This token can be saved in a secure store to be reused later
	token, err := cfg.Exchange(oauth2.NoContext, code)
	if err != nil {
		return err
	}

	// Login to the IMAP server with XOAUTH2
	saslClient := sasl.NewXoauth2Client(username, token.AccessToken)
	return c.Authenticate(saslClient)
}

func (b *GmailBackend) Initialize() error {
	pw, err := getFromKeyChain("www.google.com", b.Email)
	if err != nil {
		return err
	}
	b.Password = pw
	return nil
}

func (b *GmailBackend) Open() error {
	c, err := client.DialTLS("imap.gmail.com:993", nil)
	if err != nil {
		return err
	}

	if _, err := enable.NewClient(c).SupportEnable(); err != nil {
		return err
	}

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

	//authenticate(c, cfg, "rob@lillack.net")

	b.Client = c

	return nil
}

func (b *GmailBackend) Close() error {
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

	return b.Client.Select(name, false)
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

func (b *GmailBackend) LoadInbox() ([]*models.Message, error) {
	mbox, err := b.selectMailbox("INBOX")
	if err != nil {
		return nil, err
	}
	seqset := &imap.SeqSet{Set: []imap.Seq{{Start: uint32(1), Stop: mbox.Messages}}}
	if err != nil {
		log.Println(err)
	}

	messages := make(chan *imap.Message, 100000)
	done := make(chan error, 1)
	go func() {
		done <- b.Client.Fetch(seqset, []imap.FetchItem{GmailMessageID, imap.FetchFlags, imap.FetchInternalDate, imap.FetchRFC822Size, imap.FetchEnvelope}, messages)
	}()

	b.inbox = map[models.MessageID]*models.Message{}
	list := []*models.Message{}
	for i := range messages {
		msg := b.buildMessage(i)
		if msgID, ok := getMessageID(i); ok {
			b.inbox[msgID] = msg
		}
		list = append(list, msg)
	}

	if err := <-done; err != nil {
		return nil, fmt.Errorf("unable to fetch messages: %s", err)
	}

	return list, nil
}

func (b *GmailBackend) loadMessages(seqSet *imap.SeqSet) ([]*models.Message, error) {
	messages := make(chan *imap.Message, 10000)
	done := make(chan error, 1)
	go func() {
		done <- b.Client.Fetch(seqSet, []imap.FetchItem{GmailMessageID, imap.FetchFlags, imap.FetchInternalDate, imap.FetchRFC822Size, imap.FetchEnvelope}, messages)
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

func (b *GmailBackend) getInboxUpdates(stat *imap.MailboxStatus) ([]*models.Message, error) {
	seqset := &imap.SeqSet{Set: []imap.Seq{{Start: uint32(1), Stop: stat.Messages}}}

	messages := make(chan *imap.Message, 10000)
	done := make(chan error, 1)
	go func() {
		done <- b.Client.Fetch(seqset, []imap.FetchItem{GmailMessageID}, messages)
	}()

	fetchSet := &imap.SeqSet{}
	for i := range messages {
		if msgID, ok := getMessageID(i); ok {
			if _, known := b.inbox[msgID]; !known {
				fetchSet.AddNum(i.SeqNum)
			}
		}
	}

	if err := <-done; err != nil {
		return nil, fmt.Errorf("unable to fetch messages: %s", err)
	}

	list, err := b.loadMessages(fetchSet)
	if err != nil {
		return nil, err
	}

	for _, i := range list {
		b.inbox[i.ID] = i
	}

	return list, nil
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

	return &models.Message{
		ID:       msgID,
		Sender:   msg.Envelope.From[0].PersonalName,
		Sent:     msg.Envelope.Date,
		Size:     int(msg.Size),
		Starred:  isStarred(msg.Flags),
		Answered: isAnswered(msg.Flags),
		Status:   st,
		Subject:  msg.Envelope.Subject,
	}
}

func (b *GmailBackend) Subscribe() (<-chan events.Event, error) {
	_, err := b.selectMailbox("INBOX")
	if err != nil {
		return nil, err
	}

	idleClient := idle.NewClient(b.Client)
	updates := make(chan client.Update)
	b.Client.Updates = updates

	b.idleChannel = make(chan error, 1)
	go func() {
		b.idleChannel <- idleClient.IdleWithFallback(nil, 0)
	}()

	b.eventQueue = make(chan events.Event)

	go func() {
		for {
			select {
			case update := <-updates:
				//log.Println("New update:", update)
				b.handleServerUpdate(update)

			case err := <-b.idleChannel:
				if err != nil {
					log.Fatal(err)
				}
				log.Println("Not idling anymore")
				return
			}
		}
	}()

	/*go func() {
		for {
			time.Sleep(time.Duration(rand.Intn(1000)) * time.Millisecond)
			m := models.RandomMessage()
			m.Status = models.StatusNew
			b.events <- events.NewMessage{Message: m}
		}
	}()*/

	return b.eventQueue, nil
}

// TODO: What to do with this? Had 1yr old unsafed changes …
func (b *GmailBackend) syncInbox() error {
	mbox, err := b.selectMailbox("INBOX")
	if err != nil {
		return err
	}
	seqset := &imap.SeqSet{Set: []imap.Seq{{Start: uint32(1), Stop: mbox.Messages}}}

	messages := make(chan *imap.Message)
	done := make(chan error)
	go func() {
		done <- b.Client.Fetch(seqset, []imap.FetchItem{GmailMessageID}, messages)
	}()

	seenNow := map[models.MessageID]struct{}{}
	fetchSet := &imap.SeqSet{}
	for i := range messages {
		if msgID, ok := getMessageID(i); ok {
			if _, known := b.inbox[msgID]; !known {
				fetchSet.AddNum(i.SeqNum)
			}
			seenNow[msgID] = struct{}{}
		}
	}

	if err := <-done; err != nil {
		return fmt.Errorf("unable to fetch messages IDs: %s", err)
	}

	list, err := b.loadMessages(fetchSet)
	if err != nil {
		return err
	}

	b.inbox = map[models.MessageID]*models.Message{}
	for _, i := range list {
		b.inbox[i.ID] = i
	}

	/*removeSet := []models.MessageID{}

	for i := range b.inbox {
		if _, ok := seenNow[i]; !ok {
			removeSet = append(removeSet, i)
		}
	}

	for _, i := range list {
		b.inbox[i.ID] = i
	}*/

	return nil
}

func (b *GmailBackend) handleServerUpdate(update client.Update) {
	switch u := update.(type) {
	case *client.ExpungeUpdate:
	case *client.MessageUpdate:
		b.eventQueue <- events.NewMessage{Message: b.buildMessage(u.Message)}
	case *client.MailboxUpdate:
		if u.Mailbox.Name != "INBOX" {
			return
		}
		newMsgs, err := b.getInboxUpdates(u.Mailbox)
		if err != nil {
			log.Printf("Error getting updates: %s\n", err)
			return
		}
		for _, i := range newMsgs {
			log.Printf("New: %s\n", i.Subject)
			b.eventQueue <- events.NewMessage{Message: i}
		}
		//u.Mailbox.
		//u.Mailbox.UidValidity
		//	b.eventQueue <- events.NewMessage{Message: b.buildMessage(u.Message)}
	default:
		log.Printf("%s: %+v\n", reflect.TypeOf(u).Name(), update)
	}
}

func (b *GmailBackend) Unsubscribe() error {
	b.idleChannel <- nil

	return nil
}
