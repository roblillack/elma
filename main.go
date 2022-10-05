package main

import (
	"fmt"
	"os"
	"path"

	_ "github.com/emersion/go-pgpmail"
	_ "github.com/emersion/go-smtp"
	"github.com/mitchellh/go-homedir"
	"github.com/pelletier/go-toml"

	"github.com/roblillack/elma/backend"
	"github.com/roblillack/elma/controllers"
)

type GmailConfig struct {
	Email    string
	Password string
}
type Config struct {
	Gmail GmailConfig
}

func getBackend() backend.Backend {
	home, err := homedir.Dir()
	if err != nil {
		panic(err)
	}
	config, err := toml.LoadFile(path.Join(home, ".elmarc"))
	if err != nil && !os.IsNotExist(err) {
		panic(err)
	}

	if err != nil {
		return backend.NewFakeBackend()
	}

	if email := config.Get("gmail.email"); email != nil {
		if pw := config.Get("gmail.password"); pw != nil {
			return backend.NewGmailBackendWithPassword(email.(string), pw.(string))
		}
		return backend.NewGmailBackend(email.(string))

	}

	return backend.NewFakeBackend()
}

func main() {
	app := &controllers.Application{
		Backend: getBackend(),
		// Backend: backend.NewFakeBackend(),
	}

	if err := app.Run(); err != nil {
		panic(err)
	}

	fmt.Println("Done.")
}
