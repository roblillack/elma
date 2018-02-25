package backend

import "github.com/roblillack/elma/models"

type FakeBackend struct {
	Messages []*models.Message
}

var _ Backend = &FakeBackend{}

func NewFakeBackend() *FakeBackend {
	msgs := []*models.Message{}
	for i := 0; i < 10; i++ {
		msgs = append(msgs, models.RandomMessage())
	}

	return &FakeBackend{msgs}
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
