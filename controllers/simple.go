package controllers

import (
	"github.com/rivo/tview"
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
