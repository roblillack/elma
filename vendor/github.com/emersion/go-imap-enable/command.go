package enable

import (
	"github.com/emersion/go-imap"
)

// An ENABLE command, defined in RFC 5161 section 3.1.
type Command struct {
	Capabilities []string
}

func (cmd *Command) Command() *imap.Command {
	return &imap.Command{
		Name:      commandName,
		Arguments: imap.FormatStringList(cmd.Capabilities),
	}
}

func (cmd *Command) Parse(fields []interface{}) error {
	var err error
	cmd.Capabilities, err = imap.ParseStringList(fields)
	return err
}
