package backend

import "github.com/roblillack/elma/models"

type Backend interface {
	Initialize() error
	Open() error
	Close() error
	LoadInbox() ([]*models.Message, error)
}
