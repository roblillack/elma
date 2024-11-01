package backend

import (
	"log"

	"github.com/roblillack/elma/events"
	"github.com/roblillack/elma/models"
)

type LoggerBackend struct {
	Log     *log.Logger
	Backend Backend
}

var _ Backend = &LoggerBackend{}

func NewLogger(backend Backend, logger *log.Logger) *LoggerBackend {
	if logger == nil {
		logger = log.Default()
	}
	return &LoggerBackend{Log: logger, Backend: backend}
}

func (l *LoggerBackend) ArchiveMessage(m *models.Message) error {
	l.Log.Printf("ArchiveMessage: %s\n", m.ID)
	return l.Backend.ArchiveMessage(m)
}

func (l *LoggerBackend) Close() error {
	l.Log.Printf("Close\n")
	return l.Backend.Close()
}

func (l *LoggerBackend) DeleteMessage(m *models.Message) error {
	l.Log.Printf("DeleteMessage: %s\n", m.ID)
	return l.Backend.DeleteMessage(m)
}

func (l *LoggerBackend) Initialize() error {
	l.Log.Printf("Initialize\n")
	return l.Backend.Initialize()
}

func (l *LoggerBackend) LoadInbox() (msgs []*models.Message, events chan events.Event, err error) {
	l.Log.Printf("LoadInbox: %d msgs\n", len(msgs))
	return l.Backend.LoadInbox()
}

func (b *LoggerBackend) LoadMessageContent(m *models.Message) (*models.MessageContent, error) {
	b.Log.Printf("LoadMessageContent: %s\n", m.ID)
	return b.Backend.LoadMessageContent(m)
}

func (l *LoggerBackend) Open() error {
	l.Log.Printf("Open\n")
	return l.Backend.Open()
}

func (l *LoggerBackend) PauseEvents() {
	l.Log.Printf("PauseEvents\n")
	l.Backend.PauseEvents()
}

func (l *LoggerBackend) ResumeEvents() {
	l.Log.Printf("ResumeEvents\n")
	l.Backend.ResumeEvents()
}
