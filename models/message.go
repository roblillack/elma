package models

import (
	"time"
)

type MessageID uint64

type MessageCallback func(msg *Message, idx int)

type MessageStatus uint8

const (
	StatusRead MessageStatus = iota
	StatusNew
	StatusDeleted
	StatusArchived
)

type Message struct {
	ID        MessageID
	Sent      time.Time
	Sender    string
	Subject   string
	Size      int
	Starred   bool
	Answered  bool
	Forwarded bool
	Status    MessageStatus
	Labels    []string
	// IMAP protocol specific stuff
	UID        uint32
	SequenceID uint32
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
