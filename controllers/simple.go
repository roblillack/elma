package controllers

import (
	"github.com/rivo/tview"
	"github.com/roblillack/elma/models"
)

type Simple struct {
	Layout tview.Primitive
}

func NewSimple(layout tview.Primitive) *Simple {
	return &Simple{layout}
}

func (c *Simple) View() tview.Primitive {
	return c.Layout
}

func (c *Simple) HandleUserInterfaceEvent(event models.UserInterfaceEvent) {}
