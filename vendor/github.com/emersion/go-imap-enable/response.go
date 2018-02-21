package enable

import (
	"github.com/emersion/go-imap"
	"github.com/emersion/go-imap/responses"
)

// An ENABLE response, defined in RFC 5161 section 3.2.
type Response struct {
	Capabilities []string
}

func (r *Response) Handle(resp imap.Resp) error {
	name, fields, ok := imap.ParseNamedResp(resp)
	if !ok || name != responseName {
		return responses.ErrUnhandled
	}

	if caps, err := imap.ParseStringList(fields); err != nil {
		return err
	} else {
		r.Capabilities = append(r.Capabilities, caps...)
	}

	return nil
}

func (r *Response) WriteTo(w *imap.Writer) error {
	fields := []interface{}{responseName}
	fields = append(fields, imap.FormatStringList(r.Capabilities)...)

	resp := imap.NewUntaggedResp(fields)
	return resp.WriteTo(w)
}
