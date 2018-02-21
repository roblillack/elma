# go-oauthdialog

[![GoDoc](https://godoc.org/github.com/emersion/go-oauthdialog?status.svg)](https://godoc.org/github.com/emersion/go-oauthdialog)

A Go library to present an OAuth2 dialog to the user.

## Usage

```go
package main

import (
	"log"

	"github.com/emersion/go-oauthdialog"
	"golang.org/x/oauth2"
	"golang.org/x/oauth2/google"
)

func main() {
	conf := &oauth2.Config{
		ClientID: "CLIENT_ID",
		ClientSecret: "CLIENT_SECRET",
		Scopes: []string{"https://mail.google.com"},
		Endpoint: google.Endpoint,
	}

	code, err := oauthdialog.Open(conf)
	if err != nil {
		log.Fatal(err)
	}

	tok, err := conf.Exchange(oauth2.NoContext, code)
	if err != nil {
		log.Fatal(err)
	}

	log.Println("Token:", tok)
}
```

## License

MIT
