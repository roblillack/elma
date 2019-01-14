package controllers

import (
	"fmt"
	"sync"
	"time"

	"github.com/roblillack/elma/events"

	"github.com/gdamore/tcell"
	"github.com/rivo/tview"
	"github.com/roblillack/elma/backend"
)

type Application struct {
	Backend    backend.Backend
	View       *tview.Application
	Screens    []Controller
	screenLock sync.RWMutex
}

func (a *Application) GotoInbox() error {
	c, err := NewInbox(a)
	if err != nil {
		return err
	}

	a.ReplaceScreens(c)
	return nil
}

func (a *Application) GotoHelp() error {
	help := tview.NewTextView()
	help.SetTitle("Help").SetBorder(true).SetTitleAlign(tview.AlignLeft)
	help.SetInputCapture(func(event *tcell.EventKey) *tcell.EventKey {
		if event.Key() == tcell.KeyEsc || event.Key() == tcell.KeyEscape {
			a.PopScreen()
		}

		return event
	})
	fmt.Fprintf(help, "Hello world!\n\ntime: %s", time.Now())

	a.PushScreen(NewSimple(help))

	return nil
}

func (a *Application) PushScreen(c Controller) {
	a.screenLock.Lock()
	defer a.screenLock.Unlock()
	a.Screens = append(a.Screens, c)
	a.View.SetRoot(c.View(), true)
}

func (a *Application) ReplaceScreens(c Controller) {
	a.screenLock.Lock()
	defer a.screenLock.Unlock()
	a.Screens = []Controller{c}
	a.View.SetRoot(c.View(), true)
}

func (a *Application) PopScreen() {
	a.screenLock.Lock()
	defer a.screenLock.Unlock()

	l := len(a.Screens)
	if l == 0 {
		return
	}
	a.Screens = a.Screens[0 : l-1]

	if l <= 1 {
		a.View.Stop()
		return
	}

	a.View.SetRoot(a.Screens[l-2].View(), true)
}

func (a *Application) processEvents(backend backend.Backend, eventBus <-chan events.Event) {
	for {
		evt := <-eventBus
		a.screenLock.RLock()
		for _, screen := range a.Screens {
			if listener, ok := screen.(events.EventListener); ok {
				listener.HandleEvent(evt)
			}
		}
		a.screenLock.RUnlock()

		a.View.Draw()
	}
}

func (a *Application) Run() error {
	if a.Backend == nil {
		return fmt.Errorf("no application backend set up")
	}

	if err := a.Backend.Initialize(); err != nil {
		return err
	}

	if publisher, ok := a.Backend.(events.EventPublisher); ok {
		c, err := publisher.Subscribe()
		if err != nil {
			return err
		}
		go a.processEvents(a.Backend, c)
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
