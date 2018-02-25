package backend

import (
	"errors"
	"fmt"
	"log"
	"os/exec"
	"regexp"
	"strconv"
	"syscall"
	"time"

	"github.com/emersion/go-imap"
	compress "github.com/emersion/go-imap-compress"
	"github.com/emersion/go-imap-enable"
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
}

var _ Backend = &GmailBackend{}

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
	log.Println("Flags for INBOX:", mbox.Flags)
	log.Printf("Number of messages here: %d, unseen: %d\n", mbox.Messages, mbox.Unseen)
	// Get the last 4 messages
	/*from := uint32(1)
	to := mbox.Messages
	if mbox.Messages > 3 {
		// We're using unsigned integers here, only substract if the result is > 0
		from = mbox.Messages - 3
	}*/
	seqset := &imap.SeqSet{Set: []imap.Seq{{Start: uint32(1), Stop: mbox.Messages}}}
	//seqset, err := imap.ParseSeqSet("*")
	if err != nil {
		log.Println(err)
	}
	//seqset := new(imap.SeqSet)
	//seqset.AddRange(from, to)

	crit := imap.NewSearchCriteria()
	crit.Before = time.Now().Add(time.Hour)
	//crit.Uid.Add("*")*/

	/*	uids, err := b.Client.UidSearch(crit)
		if err != nil {
			return nil, fmt.Errorf("unable to search for all mails: %s", err)
		}
		log.Println(uids)

		seqset.AddNum(uids...)*/

	messages := make(chan *imap.Message, 10000)
	done := make(chan error, 1)
	go func() {
		done <- b.Client.Fetch(seqset, []imap.FetchItem{imap.FetchFlags, imap.FetchInternalDate, imap.FetchRFC822Size, imap.FetchEnvelope}, messages)
	}()

	list := []*models.Message{}
	for msg := range messages {
		log.Printf("%+v\n", msg)
		log.Println("* " + msg.Envelope.Subject)
		log.Println(msg.Flags)
		st := models.StatusRead
		if isUnread(msg.Flags) {
			st = models.StatusNew
		}
		list = append(list, &models.Message{
			Sender:   msg.Envelope.From[0].PersonalName,
			Sent:     msg.Envelope.Date,
			Size:     int(msg.Size),
			Starred:  isStarred(msg.Flags),
			Answered: isAnswered(msg.Flags),
			Status:   st,
			Subject:  msg.Envelope.Subject,
		})
	}

	if err := <-done; err != nil {
		return nil, fmt.Errorf("unable to fetch messages: %s", err)
	}

	return list, nil
	//return nil, errors.New("Not implemented")
}
