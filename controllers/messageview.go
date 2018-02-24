package controllers

import (
	"fmt"

	"github.com/gdamore/tcell"
	"github.com/rivo/tview"
	"github.com/roblillack/elma/models"
)

type MessageViewController struct {
	App       *Application
	TextView  *tview.TextView
	ActionBar *tview.TextView
	InfoBar   *tview.TextView
	Message   *models.Message
}

var _ Controller = &MessageViewController{}

func NewMessageView(app *Application, msg *models.Message) (Controller, error) {
	return &MessageViewController{
		App:     app,
		Message: msg,
	}, nil
}

func (c *MessageViewController) UpdateActionBar() {

}

func (c *MessageViewController) UpdateInfoBar() {

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
	fmt.Fprintln(c.TextView, c.Message.Subject)
	c.TextView.SetInputCapture(func(event *tcell.EventKey) *tcell.EventKey {
		if event.Key() == tcell.KeyEscape || event.Key() == tcell.KeyEsc || event.Key() == tcell.KeyLeft {
			c.App.PopView()
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
