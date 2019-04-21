package backend

import (
	"github.com/roblillack/elma/backend/mock"
	"github.com/roblillack/elma/events"
	"github.com/roblillack/elma/models"
)

type FakeBackend struct {
	Messages []*models.Message
	events   chan events.Event
}

var _ Backend = &FakeBackend{}
var _ events.EventPublisher = &FakeBackend{}

func NewFakeBackend() *FakeBackend {
	msgs := []*models.Message{}
	for i := 0; i < 10; i++ {
		msgs = append(msgs, models.RandomMessage())
	}

	return &FakeBackend{msgs, nil}
}

func (b *FakeBackend) Initialize() error {
	return nil
}

func (b *FakeBackend) Open() error {
	return nil
}

func (b *FakeBackend) Close() error {
	return nil
}

func (b *FakeBackend) LoadInbox() ([]*models.Message, error) {
	return b.Messages, nil
}

func (b *FakeBackend) Subscribe() (<-chan events.Event, error) {
	b.events = make(chan events.Event, 1000)

	go func() {
		for {
			time.Sleep(time.Duration(rand.Intn(1000)) * time.Millisecond)
			m := mock.RandomMessage()
			m.Status = models.StatusNew
			b.events <- events.NewMessage{Message: m}
		}
	}()

	return b.events, nil
}

func (b *FakeBackend) Unsubscribe() error {
	return nil
}
