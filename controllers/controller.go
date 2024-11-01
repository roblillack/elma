package controllers

import (
	"github.com/rivo/tview"
)

type Controller interface {
	View() tview.Primitive
}

type ParentController interface {
	Controller
	HandleChildKeypress(rune)
}

// KeymapEntry represents an entry in the keymap which can be used by the
// screen's parent to ask it to register "external" keybindings. The screen
// will then register these keybindings, but close itself when the key is
// pressed and tell the parent that the key was pressed by returning the
// key via the Pop() method.
type KeymapEntry struct {
	Key         rune
	Description string
}
