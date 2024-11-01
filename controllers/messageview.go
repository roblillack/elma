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

func renderMessage(msg *models.Message, content *models.MessageContent) (string, error) {
	if rawHTML := getContentType(content, "text/html"); rawHTML != nil {
		doc, err := html.Parse(bytes.NewReader(rawHTML))
		if err != nil {
			return "", fmt.Errorf("Failed to parse HTML: %v", err)
		}
		buf := &strings.Builder{}
		if err := formatter.Write(buf, doc, false); err != nil {
			return "", fmt.Errorf("Failed to format FTML: %v", err)
		}

		return fmt.Sprintf("From: %s\nSubject: %s\n\n%s", msg.Sender, msg.Subject, buf.String()), nil
	}

	if txt := getContentType(content, "text/plain"); txt != nil {
		return fmt.Sprintf("From: %s\nSubject: %s\n\n%s", msg.Sender, msg.Subject, string(txt)), nil
	}

	return fmt.Sprintf("From: %s\nSubject: %s\n\n???", msg.Sender, msg.Subject), nil
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
