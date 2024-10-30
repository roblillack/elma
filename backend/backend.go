package backend

import (
	"errors"

	"github.com/roblillack/elma/events"
	"github.com/roblillack/elma/models"
)

var ErrInvalidCredentials = errors.New("invalid credentials")

type Backend interface {
	Initialize() error
	Open() error
	Close() error
	LoadInbox() ([]*models.Message, chan events.Event, error)
	LoadMessageContent(*models.Message) (*models.MessageContent, error)
	PauseEvents()
	ResumeEvents()

	ArchiveMessage(*models.Message) error
	DeleteMessage(*models.Message) error
}
