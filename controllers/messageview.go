package controllers

import (
	"bytes"
	"fmt"
	"io"
	"strings"

	tcell "github.com/gdamore/tcell/v2"
	"github.com/rivo/tview"

	"github.com/roblillack/elma/models"
	"github.com/roblillack/elma/styles"
	"github.com/roblillack/elma/views"
	"github.com/roblillack/elma/views/formatters"
	"github.com/roblillack/ftml/html"
)

type MessageViewController struct {
	App         *Application
	TextView    *tview.TextView
	ActionBar   *tview.TextView
	InfoBar     *tview.TextView
	Message     *models.Message
	Content     *models.MessageContent
	Keymap      []KeymapEntry
	Unformatted bool
}

var _ Controller = &MessageViewController{}

func NewMessageView(app *Application, msg *models.Message, content *models.MessageContent, keymap []KeymapEntry) (Controller, error) {
	return &MessageViewController{
		App:     app,
		Message: msg,
		Content: content,
		Keymap:  keymap,
	}, nil
}

func (c *MessageViewController) UpdateActionBar() {
	c.ActionBar.Clear()

	b := strings.Builder{}
	b.WriteString("q:Close s:Star r:Reply f:Forward y:Archive d:Delete")
	if len(c.Keymap) > 0 {
		b.WriteString(" —")
		for _, e := range c.Keymap {
			fmt.Fprintf(&b, " %c:%s", e.Key, e.Description)
		}
	}
	fmt.Fprint(c.ActionBar, b.String())
}

func (c *MessageViewController) UpdateInfoBar() {
}

func getContentType(content *models.MessageContent, contentType string) []byte {
	for _, part := range content.Parts {
		if part.ContentType == contentType {
			return part.Content
		}
	}

	return nil
}

func writeContent(w io.Writer, content *models.MessageContent, unformatted bool) error {
	if rawHTML := getContentType(content, "text/html"); rawHTML != nil {
		if unformatted {
			if _, err := io.WriteString(w, string(rawHTML)); err != nil {
				return fmt.Errorf("Failed to write raw HTML: %v", err)
			}
			return nil
		}

		doc, err := html.Parse(bytes.NewReader(rawHTML))
		if err != nil {
			return fmt.Errorf("Failed to parse HTML: %v", err)
		}

		if err := formatters.WriteFTML(w, doc); err != nil {
			return fmt.Errorf("Failed to format FTML: %v", err)
		}

		return nil
	}

	if txt := getContentType(content, "text/plain"); txt != nil {
		if _, err := io.WriteString(w, tview.Escape(string(txt))); err != nil {
			return fmt.Errorf("Failed to write plain text: %v", err)
		}
	}

	return fmt.Errorf("No known content types found")
}

func padToWidth(s string, width int) string {
	if len(s) > width {
		return s[:width-1] + "…"
	}

	for i := len(s); i < width; i++ {
		s += " "
	}

	return s
}

func (c *MessageViewController) renderMessage(w *tview.TextView, msg *models.Message, content *models.MessageContent) error {
	width, _ := c.App.ScreenSize()

	meta := []string{
		"Date:    " + msg.Sent.Format("2006-01-02 15:04:05 -0700"),
		"From:    " + msg.Sender,
		"Subject: " + msg.Subject,
	}
	if content.Mailer != "" {
		meta = append(meta, "Mailer:  "+content.Mailer)
	}
	for _, line := range meta {
		if _, err := io.WriteString(w,
			fmt.Sprintf("[::i]%s[::I]\n",
				tview.Escape(padToWidth(line, width)))); err != nil {
			return err
		}
	}
	io.WriteString(w, "\n")

	if err := writeContent(w, content, c.Unformatted); err != nil {
		fmt.Fprintf(w, "Failed to render message content: %v", err)
		return nil
	}

	footer := &strings.Builder{}
	footer.WriteString("---\nMessage parts:\n")
	for _, parts := range content.Parts {
		fmt.Fprintf(footer, "- %s: %d bytes\n", parts.ContentType, len(parts.Content))
	}
	if _, err := io.WriteString(w, tview.Escape(footer.String())); err != nil {
		return err
	}

	return nil
}

func (c *MessageViewController) View() tview.Primitive {
	c.ActionBar = tview.NewTextView().
		SetDynamicColors(true).
		SetRegions(true).
		SetWrap(false).
		SetTextStyle(styles.ActionBarStyle)

	c.UpdateActionBar()

	c.InfoBar = tview.NewTextView().
		SetDynamicColors(true).
		SetRegions(true).
		SetWrap(false).
		SetTextStyle(styles.InfoBarStyle)

	c.TextView = tview.NewTextView().SetDynamicColors(true).SetTextStyle(styles.MessageViewStyle)
	if err := c.renderMessage(c.TextView, c.Message, c.Content); err != nil {
		fmt.Fprintf(c.TextView, "Failed to render message: %v", err)
	}
	c.TextView.SetInputCapture(func(event *tcell.EventKey) *tcell.EventKey {
		if event.Key() == tcell.KeyEscape || event.Key() == tcell.KeyEsc ||
			event.Key() == tcell.KeyLeft || event.Rune() == 'q' || event.Rune() == 'Q' {
			c.App.PopScreen()
			return nil
		}

		if event.Rune() == '.' {
			options := []views.MenuItem{}
			if c.Unformatted {
				options = append(options,
					views.MenuItem{
						Key:         'u',
						Title:       "Formatted",
						Description: "Shows formatted message content",
						OnActivate:  func() { c.Unformatted = !c.Unformatted }})
			} else {
				options = append(options,
					views.MenuItem{
						Key:         'u',
						Title:       "Unformatted",
						Description: "Shows unformatted message content",
						OnActivate:  func() { c.Unformatted = !c.Unformatted }})
			}
			c.App.ShowMenu("Message", options)
		}

		for _, e := range c.Keymap {
			if event.Rune() == e.Key {
				c.App.PopScreen(event.Rune())
				return nil
			}
		}

		return event
	})

	layout := tview.NewFlex().
		SetDirection(tview.FlexRow).
		AddItem(c.ActionBar, 1, 1, false).
		AddItem(c.TextView, 0, 1, true).
		AddItem(c.InfoBar, 1, 1, false)

	return layout
}
