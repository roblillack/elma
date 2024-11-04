package docs

import (
	"embed"
	"path/filepath"
	"slices"

	"github.com/roblillack/ftml"
)

//go:embed *.ftml
var Embed embed.FS

func Documents() ([]*ftml.Document, error) {
	entries, err := Embed.ReadDir(".")
	if err != nil {
		return nil, err
	}

	var sections []string
	for _, entry := range entries {
		if filepath.Ext(entry.Name()) != ".ftml" {
			continue
		}
		sections = append(sections, entry.Name())
	}
	slices.Sort(sections)

	var documents []*ftml.Document
	for _, section := range sections {
		f, err := Embed.Open(section)
		if err != nil {
			return nil, err
		}
		defer f.Close()
		d, err := ftml.Parse(f)
		if err != nil {
			return nil, err
		}
		documents = append(documents, d)
	}

	return documents, nil
}

func ByteSlices() ([][]byte, error) {
	entries, err := Embed.ReadDir(".")
	if err != nil {
		return nil, err
	}

	var sections []string
	for _, entry := range entries {
		if filepath.Ext(entry.Name()) != ".ftml" {
			continue
		}
		sections = append(sections, entry.Name())
	}
	slices.Sort(sections)

	var documents [][]byte
	for _, section := range sections {
		content, err := Embed.ReadFile(section)
		if err != nil {
			return nil, err
		}
		documents = append(documents, content)
	}

	return documents, nil
}
