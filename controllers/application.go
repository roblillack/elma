package controllers

import (
	"fmt"
	"time"

	"github.com/gdamore/tcell"
	"github.com/rivo/tview"
	"github.com/roblillack/elma/backend"
)

type Application struct {
	Backend *backend.GmailBackend
	View    *tview.Application
	Views   []tview.Primitive
}

func (a *Application) GotoInbox() error {
	c, err := NewInbox(a)
	if err != nil {
		return err
	}

	a.ReplaceViews(c.View())
	return nil
}

func (a *Application) GotoHelp() error {
	help := tview.NewTextView()
	help.SetTitle("Help").SetBorder(true).SetTitleAlign(tview.AlignLeft)
	help.SetInputCapture(func(event *tcell.EventKey) *tcell.EventKey {
		if event.Key() == tcell.KeyEsc || event.Key() == tcell.KeyEscape {
			a.PopView()
		}

		return event
	})
	fmt.Fprintf(help, "Hello world!\n\ntime: %s", time.Now())
	a.PushView(help)

	return nil
}

func (a *Application) PushView(p tview.Primitive) {
	a.Views = append(a.Views, p)
	a.View.SetRoot(p, true)
}

func (a *Application) ReplaceViews(p tview.Primitive) {
	a.Views = []tview.Primitive{p}
	a.View.SetRoot(p, true)
}

func (a *Application) PopView() {
	l := len(a.Views)
	if l == 0 {
		return
	}
	a.Views = a.Views[0 : l-1]

	if l <= 1 {
		a.View.Stop()
		return
	}

	a.View.SetRoot(a.Views[l-2], true)
}

func (a *Application) Run() error {
	if a.Backend == nil {
		return fmt.Errorf("no application backend set up")
	}

	if err := a.Backend.Initialize(); err != nil {
		return err
	}

	tview.Styles.PrimitiveBackgroundColor = tcell.ColorWhite
	tview.Styles.ContrastBackgroundColor = tcell.ColorDarkRed
	tview.Styles.InverseTextColor = tcell.ColorWhite
	tview.Styles.PrimaryTextColor = tcell.ColorBlack

	a.View = tview.NewApplication()
	a.View.SetInputCapture(func(event *tcell.EventKey) *tcell.EventKey {
		if event.Key() == tcell.KeyCtrlQ {
			a.View.Stop()
			return nil
		}

		if event.Key() == tcell.KeyF1 {
			a.GotoHelp()
		}

		return event
	})

	if err := a.GotoInbox(); err != nil {
		return err
	}

	return a.View.Run()
}
