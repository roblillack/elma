package compress

import (
	"compress/flate"
	"net"

	"github.com/emersion/go-imap/server"
)

type Handler struct {
	Command
}

func (h *Handler) Handle(conn server.Conn) error {
	if h.Mechanism != Deflate {
		return NotSupportedError{h.Mechanism}
	}
	return nil
}

func (h *Handler) Upgrade(conn server.Conn) error {
	err := conn.Upgrade(func(conn net.Conn) (net.Conn, error) {
		return createDeflateConn(conn, flate.DefaultCompression)
	})
	if err != nil {
		return err
	}

	return nil
}

type extension struct{}

func (ext *extension) Capabilities(c server.Conn) []string {
	return []string{Capability}
}

func (ext *extension) Command(name string) server.HandlerFactory {
	if name != commandName {
		return nil
	}

	return func() server.Handler {
		return new(Handler)
	}
}

// NewExtension creates a new COMPRESS server extension.
func NewExtension() server.Extension {
	return new(extension)
}
