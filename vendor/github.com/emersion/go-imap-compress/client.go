package compress

import (
	"compress/flate"
	"net"

	"github.com/emersion/go-imap/client"
)

// Client is a COMPRESS client.
type Client struct {
	client       *client.Client
	isCompressed bool
}

// Compress instructs the server to use the named compression mechanism for all
// commands and/or responses.
func (c *Client) Compress(mech string) error {
	if c.isCompressed {
		return ErrAlreadyEnabled
	}

	if mech != Deflate {
		return NotSupportedError{mech}
	}

	cmd := &Command{Mechanism: mech}

	err := c.client.Upgrade(func(conn net.Conn) (net.Conn, error) {
		if status, err := c.client.Execute(cmd, nil); err != nil {
			return nil, err
		} else if err := status.Err(); err != nil {
			return nil, err
		}

		return createDeflateConn(conn, flate.DefaultCompression)
	})
	if err != nil {
		return err
	}

	c.isCompressed = true
	return nil
}

// IsCompressed checks if this client's connection is compressed.
func (c *Client) IsCompress() bool {
	return c.isCompressed
}

// SupportCompress checks if the server supports a compression mechanism.
func (c *Client) SupportCompress(mech string) (bool, error) {
	return c.client.Support(Capability + "=" + mech)
}

// NewClient creates a new client.
func NewClient(c *client.Client) *Client {
	return &Client{client: c}
}
