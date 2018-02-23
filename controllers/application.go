package controllers

import (
	"fmt"
	"strings"

	"github.com/gdamore/tcell"
	"github.com/rivo/tview"
	"github.com/roblillack/elma/backend"
	"github.com/roblillack/elma/models"
	"github.com/roblillack/elma/views"
)

type Application struct {
	Backend          *backend.GmailBackend
	ActionBar        *tview.TextView
	InfoBar          *tview.TextView
	Messages         []*models.Message
	ScheduledActions []models.Action
}

func (a *Application) ScheduleAction(t models.ActionType, msg *models.Message) {
	a.ScheduledActions = append(a.ScheduledActions, models.Action{Type: t, Message: msg})
}

func (a *Application) UpdateActionBar(msg *models.Message) {
	a.ActionBar.Clear()

	b := strings.Builder{}

	b.WriteString("^Q:Quit")

	if msg == nil {
		fmt.Fprint(a.ActionBar, b.String())
		return
	}

	b.WriteString(" Enter:Open")

	if msg.Starred {
		b.WriteString(" s:Unstar")
	} else {
		b.WriteString(" s:Star")
	}

	switch msg.Status {
	case models.StatusNew:
		fallthrough
	case models.StatusRead:
		b.WriteString(" r:Reply y:Archive d:Delete")
	case models.StatusDeleted:
		b.WriteString(" r:Reply y:Archive u:Undelete")
	case models.StatusArchived:
		b.WriteString(" r:Reply u:Unarchive d:Delete")
	}

	if len(a.ScheduledActions) > 0 {
		b.WriteString(" $:Commit")
	}

	fmt.Fprint(a.ActionBar, b.String())
}

func (a *Application) UpdateInfoBar(msg *models.Message, idx int) {
	a.InfoBar.Clear()
	fmt.Fprintf(a.InfoBar, "Message %d/%d, %d scheduled actions", idx, len(a.Messages), len(a.ScheduledActions))
}

func (a *Application) Run() error {
	if a.Backend == nil {
		return fmt.Errorf("no application backend set up")
	}

	if err := a.Backend.Initialize(); err != nil {
		return err
	}

	if msgs, err := a.Backend.LoadInbox(); err != nil {
		return err
	} else {
		a.Messages = msgs
	}

	tview.Styles.PrimitiveBackgroundColor = tcell.ColorWhite
	tview.Styles.ContrastBackgroundColor = tcell.ColorDarkRed
	tview.Styles.InverseTextColor = tcell.ColorWhite
	tview.Styles.PrimaryTextColor = tcell.ColorBlack

	app := tview.NewApplication()

	a.ActionBar = tview.NewTextView().
		SetDynamicColors(true).
		SetRegions(true).
		SetWrap(false)

	a.ActionBar.SetTextColor(tcell.ColorBlack).
		SetBackgroundColor(tcell.ColorYellow)

	a.UpdateActionBar(nil)

	a.InfoBar = tview.NewTextView().
		SetDynamicColors(true).
		SetRegions(true).
		SetWrap(false)

	a.InfoBar.SetTextColor(tcell.ColorBlack).
		SetBackgroundColor(tcell.ColorYellow)

	messageList := views.NewMessageList()

	messageList.
		SetMessages(a.Messages).
		OnSelectionChanged(func(msg *models.Message, idx int) {
			a.UpdateActionBar(msg)
			a.UpdateInfoBar(msg, idx)
		}).
		OnDeleteMessage(func(msg *models.Message, idx int) {
			msg.Status = models.StatusDeleted
			messageList.UpdateMessage(idx, msg)
			a.ScheduleAction(models.TypeDelete, msg)
			messageList.Select(idx + 1)
		}).
		OnArchiveMessage(func(msg *models.Message, idx int) {
			msg.Status = models.StatusArchived
			messageList.UpdateMessage(idx, msg)
			a.ScheduleAction(models.TypeArchive, msg)
			messageList.Select(idx + 1)
		}).
		OnUndoMessageAction(func(msg *models.Message, idx int) {
			t := models.TypeMarkAsRead
			if msg.Status == models.StatusNew || msg.Status == models.StatusRead {
				msg.Status = models.StatusNew
				t = models.TypeMoveToInboxUnread
			} else {
				msg.Status = models.StatusRead
				t = models.TypeMoveToInboxRead
			}
			messageList.UpdateMessage(idx, msg)
			a.ScheduleAction(t, msg)
			messageList.Select(idx + 1)
		}).
		OnStarMessageAction(func(msg *models.Message, idx int) {
			msg.Starred = !msg.Starred
			messageList.UpdateMessage(idx, msg)
			t := models.TypeMarkAsStarred
			if !msg.Starred {
				t = models.TypeMarkAsUnstarred
			}
			a.ScheduleAction(t, msg)
			messageList.Select(idx + 1)
		}).
		Select(0)

	layout := tview.NewFlex().
		SetDirection(tview.FlexRow).
		AddItem(a.ActionBar, 1, 1, false).
		AddItem(messageList, 0, 1, true).
		AddItem(a.InfoBar, 1, 1, false)

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

	return app.SetRoot(layout, true).SetFocus(layout).Run()
}
