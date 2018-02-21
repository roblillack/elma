package oauthdialog

import (
	"crypto/rand"
	"encoding/base64"
)

const stateLength = 32

func randomString(n int) (string, error) {
	b := make([]byte, n)
	_, err := rand.Read(b)
	if err != nil {
		return "", err
	}

	return base64.RawURLEncoding.EncodeToString(b), nil
}

func generateState() (string, error) {
	return randomString(stateLength)
}
