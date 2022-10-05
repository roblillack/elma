package mock

import (
	"math/rand"
	"time"

	lorem "github.com/drhodes/golorem"

	"github.com/roblillack/elma/models"
)

func randomString(options ...string) string {
	return options[rand.Intn(len(options))]
}

type Mocker struct {
	lastUID uint32
}

func New() *Mocker {
	return &Mocker{
		lastUID: uint32(rand.Int31n(1000)),
	}
}

func (m *Mocker) RandomMessage() *models.Message {
	m.lastUID += 1
	return &models.Message{
		Sent: time.Now(),
		Sender: randomString("Anton", "Bertram", "Chris", "David", "Emil",
			"Frank", "Gert", "Hugh", "Ian", "John", "Kevin", "Loren", "Maggy", "Nora",
			"Oprah", "Peter", "Quentin", "Robert", "Susan", "Trevor", "Uma", "Victoria",
			"Wilma", "Xynthia", "Yves", "Ziggy") + " " +
			randomString("Achilles", "Johnson", "Mustermann", "Mueller", "Østerberg", "Smith"),
		Subject: lorem.Sentence(3, 15),
		Size:    rand.Intn(7203680) + 200,
		Status:  models.StatusNew,
		UID:     m.lastUID,
	}
}

func (m *Mocker) OldRandomMessage() *models.Message {
	msg := m.RandomMessage()
	msg.Sent = time.Now().Add(-time.Hour*time.Duration(rand.Intn(1000)) - time.Duration(rand.Intn(60))*time.Minute)
	msg.Starred = rand.Intn(10) == 0
	msg.Answered = rand.Intn(7) == 0
	msg.Forwarded = rand.Intn(25) == 0
	msg.Status = models.StatusRead
	if rand.Intn(20) == 0 {
		msg.Status = models.StatusNew
	}
	return msg
}
