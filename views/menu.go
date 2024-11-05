package views

import (
	"github.com/gdamore/tcell/v2"
	"github.com/rivo/tview"
)

type MenuItem struct {
	Key         rune
	Title       string
	Description string
	OnActivate  func()
}

func Modal(p tview.Primitive, width, height int) tview.Primitive {
	return tview.NewFlex().
		AddItem(nil, 0, 1, false).
		AddItem(tview.NewFlex().SetDirection(tview.FlexRow).AddItem(nil, 0, 1, false).AddItem(p, height, 2, true).AddItem(nil, 0, 1, false), width, 2, true).
		AddItem(nil, 0, 1, false)
}

func Menu(title string, items []MenuItem, onClose func()) tview.Primitive {
	// box := tview.NewBox()
	// box.SetBorder(true)
	// box.SetTitle(title)

	list := tview.NewList()
	list.SetTitle(title).
		SetBorder(true).
		SetTitleAlign(tview.AlignLeft).
		SetBorderStyle(tcell.StyleDefault).
		SetTitleColor(tcell.ColorDefault)

	for _, item := range items {
		list.AddItem(item.Title, item.Description, item.Key, item.OnActivate)
	}
	list.SetInputCapture(func(event *tcell.EventKey) *tcell.EventKey {
		if event.Key() == tcell.KeyEsc || event.Key() == tcell.KeyEscape {
			if onClose != nil {
				onClose()
			}

			return nil
		}
		return event
	})
	return Modal(list, 40, 20)
}
