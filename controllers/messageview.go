package controllers

import (
	"github.com/rivo/tview"
	"github.com/roblillack/elma/models"
)

type MessageViewController struct {
	App       *Application
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

func (c *MessageViewController) View() tview.Primitive {
	return nil
}
