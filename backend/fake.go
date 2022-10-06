package backend

import (
	"math/rand"
	"time"

	"github.com/roblillack/elma/backend/mock"
	"github.com/roblillack/elma/events"
	"github.com/roblillack/elma/models"
)

type FakeBackend struct {
	Messages []*models.Message
	Mocker   *mock.Mocker
	events   chan events.Event
}

var _ Backend = &FakeBackend{}
var _ events.EventPublisher = &FakeBackend{}

func NewFakeBackend() *FakeBackend {
	mocker := mock.New()
	msgs := []*models.Message{}
	for i := 0; i < 500; i++ {
		msgs = append(msgs, mocker.OldRandomMessage())
	}

	return &FakeBackend{msgs, mocker, nil}
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

func (b *FakeBackend) PauseEvents() {
}

func (b *FakeBackend) ResumeEvents() {
}

func (b *FakeBackend) LoadInbox() ([]*models.Message, chan events.Event, error) {
	ch := make(chan events.Event, 1000)

	go func() {
		for {
			time.Sleep(time.Duration(rand.Intn(1000)) * time.Millisecond)
			m := b.Mocker.RandomMessage()
			m.Status = models.StatusNew
			b.events <- events.NewMessage{Message: m}
		}
	}()

	return b.Messages, ch, nil
}

func (b *FakeBackend) Subscribe() (<-chan events.Event, error) {
	b.events = make(chan events.Event, 1000)

	// go func() {
	// 	for {
	// 		time.Sleep(time.Duration(rand.Intn(1000)) * time.Millisecond)
	// 		m := mock.RandomMessage()
	// 		m.Status = models.StatusNew
	// 		b.events <- events.NewMessage{Message: m}
	// 	}
	// }()

	return b.events, nil
}

func (b *FakeBackend) Unsubscribe() error {
	return nil
}

func (b *FakeBackend) ArchiveMessage(*models.Message) error {
	return nil
}

func (b *FakeBackend) DeleteMessage(*models.Message) error {
	return nil
}
