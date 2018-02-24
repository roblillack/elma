package models

import (
	"math/rand"
	"time"

	lorem "github.com/drhodes/golorem"
)

type MessageCallback func(msg *Message, idx int)

type MessageStatus uint8

const (
	StatusRead MessageStatus = iota
	StatusNew
	StatusDeleted
	StatusArchived
)

type Message struct {
	Sent      time.Time
	Sender    string
	Subject   string
	Size      int
	Starred   bool
	Answered  bool
	Forwarded bool
	Status    MessageStatus
}

func (m *Message) FlagString() string {
	str := []rune{' ', ' ', ' '}
	switch m.Status {
	case StatusArchived:
		str[0] = 'A'
	case StatusDeleted:
		str[0] = 'D'
	case StatusNew:
		str[0] = 'N'
	}
	if m.Starred {
		str[1] = '*'
	}
	if m.Forwarded && m.Answered {
		str[2] = '⇄'
	} else if m.Forwarded {
		str[2] = '→'
	} else if m.Answered {
		str[2] = '↩'
	}
	return string(str)
}

func randomString(options ...string) string {
	return options[rand.Intn(len(options))]
}

func RandomMessage() *Message {
	s := StatusRead
	if rand.Intn(10) == 0 {
		s = StatusNew
	}
	return &Message{
		Sent: time.Now().Add(-time.Hour*time.Duration(rand.Intn(1000)) - time.Duration(rand.Intn(60))*time.Minute),
		Sender: randomString("Anton", "Bertram", "Chris", "David", "Emil", "Frank", "Gert", "Hugh", "Ian", "John", "Kevin") + " " +
			randomString("Achilles", "Johnson", "Mustermann", "Mueller", "Østerberg", "Smith"),
		Subject:  lorem.Sentence(3, 15),
		Size:     rand.Intn(7203680) + 200,
		Starred:  rand.Intn(10) == 0,
		Answered: rand.Intn(8) == 0,
		Status:   s,
	}
}
