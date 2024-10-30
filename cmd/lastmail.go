package main

import (
	"bytes"
	"flag"
	"fmt"
	"log"
	"math/rand"
	"os"
	"path"
	"strings"

	_ "github.com/emersion/go-pgpmail"
	_ "github.com/emersion/go-smtp"
	"github.com/mitchellh/go-homedir"
	"github.com/pelletier/go-toml"

	"github.com/roblillack/elma/backend"
	"github.com/roblillack/elma/models"
	"github.com/roblillack/ftml/formatter"
	"github.com/roblillack/ftml/html"
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

func getContentType(content *models.MessageContent, contentType string) []byte {
	for _, part := range content.Parts {
		if part.ContentType == contentType {
			return part.Content
		}
	}

	return nil
}

func formatMail(content *models.MessageContent) string {
	if rawHTML := getContentType(content, "text/html"); rawHTML != nil {
		fmt.Println(string(rawHTML))
		fmt.Println("--------------")
		doc, err := html.Parse(bytes.NewReader(rawHTML))
		if err != nil {
			return fmt.Sprintf("Failed to parse HTML: %v", err)
		}
		buf := &strings.Builder{}
		if err := formatter.Write(buf, doc, false); err != nil {
			return fmt.Sprintf("Failed to format FTML: %v", err)
		}

		return buf.String()
	}

	if txt := getContentType(content, "text/plain"); txt != nil {
		return string(txt)
	}

	return ""
}

func main() {
	flag.Parse()

	beLog, err := os.Create("backend.log")
	if err != nil {
		panic(err)
	}
	defer beLog.Close()

	b := backend.NewLogger(getBackend(), log.New(beLog, "", log.LstdFlags))
	msgs, _, err := b.LoadInbox()
	if err != nil {
		log.Fatalf("LoadInbox failed: %v", err)
	}
	lastMsg := msgs[rand.Intn(len(msgs))]
	fmt.Printf("Last message: %s\n", lastMsg.Subject)
	content, err := b.LoadMessageContent(lastMsg)
	if err != nil {
		log.Fatalf("LoadMessageContent failed: %v", err)
	}

	fmt.Println(formatMail(content))
}
