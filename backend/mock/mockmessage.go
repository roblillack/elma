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

func RandomMessage() *models.Message {
	s := models.StatusRead
	if rand.Intn(10) == 0 {
		s = models.StatusNew
	}
	return &models.Message{
		Sent: time.Now().Add(-time.Hour*time.Duration(rand.Intn(1000)) - time.Duration(rand.Intn(60))*time.Minute),
		Sender: randomString("Anton", "Bertram", "Chris", "David", "Emil",
			"Frank", "Gert", "Hugh", "Ian", "John", "Kevin", "Loren", "Maggy", "Nora",
			"Oprah", "Peter", "Quentin", "Robert", "Susan", "Trevor", "Uma", "Victoria",
			"Wilma", "Xynthia", "Yves", "Ziggy") + " " +
			randomString("Achilles", "Johnson", "Mustermann", "Mueller", "Østerberg", "Smith"),
		Subject:  lorem.Sentence(3, 15),
		Size:     rand.Intn(7203680) + 200,
		Starred:  rand.Intn(10) == 0,
		Answered: rand.Intn(8) == 0,
		Status:   s,
	}
}
