package controllers

import (
	"fmt"
	"strings"

	"github.com/gdamore/tcell"
	"github.com/rivo/tview"
	"github.com/roblillack/elma/models"
	"github.com/roblillack/elma/views"
)

type InboxController struct {
	App              *Application
	ActionBar        *tview.TextView
	InfoBar          *tview.TextView
	MessageList      *views.MessageList
	Messages         []*models.Message
	ScheduledActions []models.Action
}

func (a *InboxController) ScheduleAction(t models.ActionType, msg *models.Message) {
	a.ScheduledActions = append(a.ScheduledActions, models.Action{Type: t, Message: msg})
}

func (a *InboxController) UpdateActionBar(msg *models.Message) {
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

func (a *InboxController) UpdateInfoBar(msg *models.Message, idx int) {
	a.InfoBar.Clear()
	fmt.Fprintf(a.InfoBar, "Message %d/%d, %d scheduled actions", idx, len(a.Messages), len(a.ScheduledActions))
}

func (c *InboxController) handleKeyEvent(event *tcell.EventKey) *tcell.EventKey {
	key := event.Key()
	r := event.Rune()

	msg, idx := c.MessageList.SelectedMessage()

	if key == tcell.KeyBackspace || key == tcell.KeyBackspace2 || key == tcell.KeyBS || key == tcell.KeyDEL || key == tcell.KeyDelete || r == 'd' || r == 'D' {
		msg.Status = models.StatusDeleted
		c.MessageList.UpdateMessage(idx, msg)
		c.ScheduleAction(models.TypeDelete, msg)
		c.MessageList.Select(idx + 1)

		return nil
	}

	if r == 'y' || r == 'Y' {
		msg.Status = models.StatusArchived
		c.MessageList.UpdateMessage(idx, msg)
		c.ScheduleAction(models.TypeArchive, msg)
		c.MessageList.Select(idx + 1)

		return nil
	}

	if r == 's' || r == 'S' {
		msg.Starred = !msg.Starred
		c.MessageList.UpdateMessage(idx, msg)
		t := models.TypeMarkAsStarred
		if !msg.Starred {
			t = models.TypeMarkAsUnstarred
		}
		c.ScheduleAction(t, msg)
		c.MessageList.Select(idx + 1)

		return nil
	}

	if r == 'u' || r == 'U' {
		t := models.TypeMarkAsRead
		if msg.Status == models.StatusNew || msg.Status == models.StatusRead {
			msg.Status = models.StatusNew
			t = models.TypeMoveToInboxUnread
		} else {
			msg.Status = models.StatusRead
			t = models.TypeMoveToInboxRead
		}
		c.MessageList.UpdateMessage(idx, msg)
		c.ScheduleAction(t, msg)
		c.MessageList.Select(idx + 1)

		return nil
	}

	if key == tcell.KeyEnter || key == tcell.KeyRight {
		mv, err := NewMessageView(c.App, msg)
		if err != nil {
			panic(err)
		}

		c.App.PushView(mv.View())
		return nil
	}

	if r == '$' {
		msgs := make([]*models.Message, 0, len(c.Messages))
		msgCounter := 0
		newSelectionIdx := -1
		for _, i := range c.Messages {
			keep := true
			for _, act := range c.ScheduledActions {
				if (act.Type == models.TypeArchive || act.Type == models.TypeDelete) && i == act.Message {
					keep = false
					break
				}
			}

			if keep {
				if i == msg {
					newSelectionIdx = msgCounter
				}
				msgs = append(msgs, i)
				msgCounter++
			}
		}
		c.ScheduledActions = []models.Action{}
		c.Messages = msgs
		c.MessageList.SetMessages(msgs)
		if newSelectionIdx != -1 {
			c.MessageList.Select(newSelectionIdx)
		} else {
			c.MessageList.Select(idx)
		}
	}

	return event
}

func (a *InboxController) Init() (tview.Primitive, error) {
	if msgs, err := a.App.Backend.LoadInbox(); err != nil {
		return nil, err
	} else {
		a.Messages = msgs
	}

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

	a.MessageList = views.NewMessageList()
	a.MessageList.
		SetMessages(a.Messages).
		OnSelectionChanged(func(msg *models.Message, idx int) {
			a.UpdateActionBar(msg)
			a.UpdateInfoBar(msg, idx)
		}).
		Select(0)
	a.MessageList.SetInputCapture(a.handleKeyEvent)

	layout := tview.NewFlex().
		SetDirection(tview.FlexRow).
		AddItem(a.ActionBar, 1, 1, false).
		AddItem(a.MessageList, 0, 1, true).
		AddItem(a.InfoBar, 1, 1, false)

	return layout, nil
}
