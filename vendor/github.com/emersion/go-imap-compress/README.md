# go-imap-compress

[![GoDoc](https://godoc.org/github.com/emersion/go-imap-compress?status.svg)](https://godoc.org/github.com/emersion/go-imap-compress)

The [IMAP COMPRESS Extension](https://tools.ietf.org/html/rfc4978) for [go-imap](https://github.com/emersion/go-imap)

## Usage

```go
package main

import (
		"log"

		"github.com/emersion/go-imap/client"
		"github.com/emersion/go-imap-compress"
)

func main() {
	log.Println("Connecting to server...")

	// Connect to server
	c, err := client.DialTLS("mail.example.org:993", nil)
	if err != nil {
		log.Fatal(err)
	}
	log.Println("Connected")

	// Don't forget to logout
	defer c.Logout()

	// Login
	if err := c.Login("username", "password"); err != nil {
		log.Fatal(err)
	}

	// Enable compression if possible
	comp := compress.NewClient(c)
	if ok, err := comp.SupportCompress(compress.Deflate); err != nil {
		log.Fatal(err)
	} else if ok {
		if err := comp.Compress(compress.Deflate); err != nil {
			log.Fatal(err)
		} else {
			log.Printf("Compression enabled: %t", comp.IsCompress())
		}
	}

	// Continue using c with compression enabled
}
```

## License

MIT
