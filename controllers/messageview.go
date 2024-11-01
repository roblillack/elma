package controllers

import (
	"bytes"
	"fmt"
	"strings"

	tcell "github.com/gdamore/tcell/v2"
	"github.com/rivo/tview"

	"github.com/roblillack/elma/models"
	"github.com/roblillack/ftml/formatter"
	"github.com/roblillack/ftml/html"
)

type MessageViewController struct {
	App       *Application
	TextView  *tview.TextView
	ActionBar *tview.TextView
	InfoBar   *tview.TextView
	Message   *models.Message
	Content   *models.MessageContent
}

var _ Controller = &MessageViewController{}

func NewMessageView(app *Application, msg *models.Message, content *models.MessageContent) (Controller, error) {
	return &MessageViewController{
		App:     app,
		Message: msg,
		Content: content,
	}, nil
}

func (c *MessageViewController) UpdateActionBar() {
	c.ActionBar.Clear()

	b := strings.Builder{}
	b.WriteString("Esc:Close")
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

func renderContent(content *models.MessageContent) string {
	if rawHTML := getContentType(content, "text/html"); rawHTML != nil {
		doc, err := html.Parse(bytes.NewReader(rawHTML))
		if err != nil {
			return fmt.Sprintf("Failed to parse HTML: %v", err)
		}
		buf := &strings.Builder{}
		if err := formatter.Write(buf, doc, false); err != nil {
			return fmt.Sprintf("Failed to format FTML: %v", err)
		}

		return buf.String()
	}

	if txt := getContentType(content, "text/plain"); txt != nil {
		return string(txt)
	}

	return "???"
}

func renderMessage(msg *models.Message, content *models.MessageContent) (string, error) {
	footer := &strings.Builder{}
	footer.WriteString("---\nMessage parts:\n")
	for _, parts := range content.Parts {
		fmt.Fprintf(footer, "- %s: %d bytes\n", parts.ContentType, len(parts.Content))
	}

	txt := renderContent(content)

	return fmt.Sprintf("From: %s\nSubject: %s\n\n%s\n\n%s", msg.Sender, msg.Subject, txt, footer.String()), nil
}

func (c *MessageViewController) View() tview.Primitive {
	c.ActionBar = tview.NewTextView().
		SetDynamicColors(true).
		SetRegions(true).
		SetWrap(false)

	c.ActionBar.SetTextColor(tcell.ColorBlack).
		SetBackgroundColor(tcell.ColorYellow)

	c.UpdateActionBar()

	c.InfoBar = tview.NewTextView().
		SetDynamicColors(true).
		SetRegions(true).
		SetWrap(false)

	c.InfoBar.SetTextColor(tcell.ColorBlack).
		SetBackgroundColor(tcell.ColorYellow)

	c.TextView = tview.NewTextView()
	txt, err := renderMessage(c.Message, c.Content)
	if err != nil {
		txt = fmt.Sprintf("Failed to render message: %v", err)
	}
	fmt.Fprintln(c.TextView, txt)
	c.TextView.SetInputCapture(func(event *tcell.EventKey) *tcell.EventKey {
		if event.Key() == tcell.KeyEscape || event.Key() == tcell.KeyEsc || event.Key() == tcell.KeyLeft {
			c.App.PopScreen()
			return nil
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
