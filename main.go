package main

import (
	"flag"
	"fmt"
	"log"
	"os"
	"path"

	_ "github.com/emersion/go-pgpmail"
	_ "github.com/emersion/go-smtp"
	"github.com/mitchellh/go-homedir"
	"github.com/pelletier/go-toml"

	"github.com/roblillack/elma/backend"
	"github.com/roblillack/elma/controllers"
)

var demoMode = flag.Bool("D", false, "Run in demo mode, which allows you to play with the UI without configuring an actual account.")

type GmailConfig struct {
	Email    string
	Password string
}
type Config struct {
	Gmail GmailConfig
}

func getBackend() backend.Backend {
	if *demoMode {
		fmt.Println("Running in demo mode.")
		return backend.NewFakeBackend()
	}

	home, err := homedir.Dir()
	if err != nil {
		panic(err)
	}
	config, err := toml.LoadFile(path.Join(home, ".elmarc"))
	if err != nil && os.IsNotExist(err) {
		fmt.Println("No configuration file found. Running in demo mode.")
		return backend.NewFakeBackend()
	}

	if err != nil {
		panic(err)
	}

	if email := config.Get("gmail.email"); email != nil {
		if pw := config.Get("gmail.password"); pw != nil {
			return backend.NewGmailBackendWithPassword(email.(string), pw.(string))
		}

		return backend.NewGmailBackend(email.(string))
	}

	panic("No email account configured.")
}

func main() {
	flag.Parse()

	beLog, err := os.Create("backend.log")
	if err != nil {
		panic(err)
	}
	defer beLog.Close()

	app := &controllers.Application{
		Backend: backend.NewLogger(getBackend(), log.New(beLog, "", log.LstdFlags)),
	}

	if err := app.Run(); err != nil {
		panic(err)
	}

	fmt.Println("Done.")
}
