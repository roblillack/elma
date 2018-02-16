package main

import (
	"fmt"

	_ "github.com/emersion/go-imap"
	_ "github.com/emersion/go-pgpmail"
	_ "github.com/emersion/go-smtp"
	"github.com/gdamore/tcell"
	"github.com/rivo/tview"
	_ "github.com/tmc/keyring"
)

func main() {
	app := tview.NewApplication()

	actionBar := tview.NewTextView().
		SetDynamicColors(true).
		SetRegions(true).
		SetWrap(false)

	fmt.Fprintf(actionBar, "^Q:Quit")

	layout := tview.NewFlex().
		SetDirection(tview.FlexRow).
		AddItem(actionBar, 1, 1, false).
		AddItem(tview.NewTextView().SetTitle("Hello World"), 0, 1, true)

	// Shortcuts to navigate the slides.
	app.SetInputCapture(func(event *tcell.EventKey) *tcell.EventKey {
		/*if event.Key() == tcell.KeyCtrlN {
			nextSlide()
		} else if event.Key() == tcell.KeyCtrlP {
			previousSlide()
		}*/
		if event.Key() == tcell.KeyCtrlQ {
			app.Stop()
		}
		return event
	})

	// Start the application.
	if err := app.SetRoot(layout, true).SetFocus(layout).Run(); err != nil {
		panic(err)
	}
}
