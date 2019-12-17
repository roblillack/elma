package credentials

import (
	"errors"
	"fmt"
	"os/exec"
	"regexp"
	"strconv"
	"syscall"
)

func Get(service, username string) (string, error) {
	ErrNotFound := errors.New("Password not found")
	pwRe := regexp.MustCompile(`password:\s+(?:0x[A-Fa-f0-9]+\s+)?"(.+)"`)
	escapeCodeRegexp := regexp.MustCompile(`\\([0-3][0-7]{2})`)

	unescapeOne := func(code []byte) []byte {
		i, _ := strconv.ParseUint(string(code[1:]), 8, 8)
		return []byte{byte(i)}
	}

	unescape := func(raw string) string {
		if !escapeCodeRegexp.MatchString(raw) {
			return raw
		} else {
			return string(escapeCodeRegexp.ReplaceAllFunc([]byte(raw), unescapeOne))
		}
	}

	args := []string{"find-internet-password", "-s", service, "-a", username, "-g"}
	c := exec.Command("/usr/bin/security", args...)
	o, err := c.CombinedOutput()
	if err != nil {
		exitCode := -1
		if c.ProcessState != nil && c.ProcessState.Sys() != nil {
			exitCode = c.ProcessState.Sys().(syscall.WaitStatus).ExitStatus()
		}
		// check particular exit code
		if exitCode == 44 {
			return "", ErrNotFound
		}
		return "", fmt.Errorf("/usr/bin/security: %s", err)
	}
	matches := pwRe.FindStringSubmatch(string(o))
	if len(matches) != 2 {
		return "", ErrNotFound
	}
	return unescape(matches[1]), nil
}
