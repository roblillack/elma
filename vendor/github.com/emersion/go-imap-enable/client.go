package enable

import (
	"github.com/emersion/go-imap"
	"github.com/emersion/go-imap/client"
)

type Client struct {
	c *client.Client
}

func NewClient(c *client.Client) *Client {
	return &Client{c: c}
}

func (c *Client) SupportEnable() (bool, error) {
	return c.c.Support(Capability)
}

func (c *Client) Enable(caps []string) ([]string, error) {
	if c.c.State()&imap.AuthenticatedState == 0 {
		return nil, client.ErrNotLoggedIn
	}

	cmd := &Command{Capabilities: caps}
	res := &Response{}

	if status, err := c.c.Execute(cmd, res); err != nil {
		return nil, err
	} else {
		return res.Capabilities, status.Err()
	}
}
