package formatters

import "fmt"

// FormatSize returns a 4-character strings for any number of bytes gives
func FormatSize(bytes int) string {
	if bytes < 1000 {
		return fmt.Sprintf("%3dB", bytes)
	}

	for _, unit := range []rune{'K', 'M', 'G', 'T'} {
		if bytes < 10*1000 {
			return fmt.Sprintf("%0.1f%c", float64(bytes)/1000.0, unit)
		}
		bytes /= 1000
		if bytes < 1000 {
			return fmt.Sprintf("%3d%c", bytes, unit)
		}
	}

	return "????"
}
