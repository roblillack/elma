package controllers

import "github.com/rivo/tview"

type Controller interface {
	View() tview.Primitive
}
