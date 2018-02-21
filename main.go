package main

import (
	"fmt"

	_ "github.com/emersion/go-pgpmail"
	_ "github.com/emersion/go-smtp"
	"github.com/gdamore/tcell"
	"github.com/rivo/tview"
	"github.com/roblillack/mail/backend"
	"github.com/roblillack/mail/models"
	"github.com/roblillack/mail/views"
)

func main() {
	tview.Styles.PrimitiveBackgroundColor = tcell.ColorWhite
	tview.Styles.ContrastBackgroundColor = tcell.ColorDarkRed
	tview.Styles.InverseTextColor = tcell.ColorWhite
	tview.Styles.PrimaryTextColor = tcell.ColorBlack

	gmail := backend.NewGmailBackend("rob@lillack.net")
	if err := gmail.Initialize(); err != nil {
		panic(err)
	}

	app := tview.NewApplication()

	actionBar := tview.NewTextView().
		SetDynamicColors(true).
		SetRegions(true).
		SetWrap(false)

	actionBar.SetTextColor(tcell.ColorBlack).
		SetBackgroundColor(tcell.ColorYellow)

	fmt.Fprintf(actionBar, "^Q:Quit")

	/*messages := []*models.Message{}

	for i := 0; i < 1000; i++ {
		messages = append(messages, models.RandomMessage())
	}*/
	messages, err := gmail.LoadInbox()
	if err != nil {
		panic(err)
	}

	infoBar := tview.NewTextView().
		SetDynamicColors(true).
		SetRegions(true).
		SetWrap(false)

	infoBar.SetTextColor(tcell.ColorBlack).
		SetBackgroundColor(tcell.ColorYellow)

	messageList := views.NewMessageList()

	messageList.
		SetMessages(messages).
		OnSelectionChanged(func(msg *models.Message, idx int) {
			infoBar.Clear()
			fmt.Fprintln(infoBar, msg.Subject)
		}).
		OnDeleteMessage(func(msg *models.Message, idx int) {
			msg.Status = models.StatusDeleted
			messageList.UpdateMessage(idx, msg)
			messageList.Select(idx + 1)
		}).
		OnArchiveMessage(func(msg *models.Message, idx int) {
			infoBar.Clear()
			fmt.Fprintln(infoBar, "Archiving %s …", msg.Subject)
			msg.Status = models.StatusArchived
			messageList.UpdateMessage(idx, msg)
			messageList.Select(idx + 1)
		}).
		OnUndoMessageAction(func(msg *models.Message, idx int) {
			if msg.Status == models.StatusNew || msg.Status == models.StatusRead {
				msg.Status = models.StatusNew
			} else {
				msg.Status = models.StatusRead
			}
			messageList.UpdateMessage(idx, msg)
			messageList.Select(idx + 1)
		}).
		Select(0)

	layout := tview.NewFlex().
		SetDirection(tview.FlexRow).
		AddItem(actionBar, 1, 1, false).
		AddItem(messageList, 0, 1, true).
		AddItem(infoBar, 1, 1, false)

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
