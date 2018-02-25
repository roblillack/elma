package events

import (
	"github.com/roblillack/elma/models"
)

type Event interface {
}

type EventListener interface {
	HandleEvent(evt Event)
}

type EventPublisher interface {
	Subscribe() (<-chan Event, error)
	Unsubscribe() error
}

type NewMessage struct {
	Message *models.Message
}
