// The IMAP COMPRESS Extension, as defined in RFC 4978.
package compress

import (
	"errors"
)

// The COMPRESS capability.
const Capability = "COMPRESS"

// The COMPRESS command name.
const commandName = "COMPRESS"

// The DEFLATE algorithm, defined in RFC 1951.
const Deflate = "DEFLATE"

// ErrAlreadyEnabled is returned by Client.Compress when compression has
// already been enabled on the client.
var ErrAlreadyEnabled = errors.New("COMPRESS is already enabled")

// A NotSupportedError is returned by Client.Compress when the provided
// compression mechanism is not supported.
type NotSupportedError struct {
	Mechanism string
}

// Error implements error.
func (err NotSupportedError) Error() string {
	return "COMPRESS mechanism " + err.Mechanism + " not supported"
}
