package main

import (
	"fmt"

	_ "github.com/emersion/go-pgpmail"
	_ "github.com/emersion/go-smtp"
	"github.com/roblillack/elma/backend"
	"github.com/roblillack/elma/controllers"
)

func main() {
	app := &controllers.Application{
		Backend: backend.NewGmailBackend("rob@lillack.net"),
		//Backend: backend.NewFakeBackend(),
	}

	if err := app.Run(); err != nil {
		panic(err)
	}

	fmt.Println("Done.")
}
