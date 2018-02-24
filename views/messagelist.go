package views

import (
	"fmt"

	"github.com/gdamore/tcell"
	"github.com/rivo/tview"
	"github.com/roblillack/elma/models"
	"github.com/roblillack/elma/views/formatters"
)

type MessageList struct {
	*tview.Table

	messages           []*models.Message
	onSelectionChanged models.MessageCallback
}

func NewMessageList() *MessageList {
	return &MessageList{Table: tview.NewTable().SetBorders(false).SetSelectable(true, false)}
}

func (l *MessageList) Select(index int) *MessageList {
	if index >= len(l.messages) {
		index = len(l.messages) - 1
	}
	l.Table.Select(index, 0)
	if l.onSelectionChanged != nil {
		l.onSelectionChanged(l.messages[index], index)
	}
	return l
}

func (l *MessageList) SetMessages(messages []*models.Message) *MessageList {
	l.messages = append([]*models.Message{}, messages...)

	l.Table.Clear()
	l.updateCells(0, len(messages))

	return l
}

func (l *MessageList) updateCells(startRow, stopRow int) {
	for idx := startRow; idx < stopRow; idx++ {
		msg := l.messages[idx]
		fg := tview.Styles.PrimaryTextColor
		if msg.Status == models.StatusNew {
			fg = tcell.ColorRed
		} else if msg.Status == models.StatusArchived {
			fg = tcell.ColorDarkCyan
		} else if msg.Status == models.StatusDeleted {
			fg = tcell.ColorDimGray
		}
		l.Table.SetCell(idx, 0, tview.NewTableCell(msg.FlagString()).SetMaxWidth(3).SetTextColor(fg))
		l.Table.SetCell(idx, 1, tview.NewTableCell(msg.Sent.Format("[Jan 02 15:04]")).SetTextColor(fg))
		l.Table.SetCell(idx, 2, tview.NewTableCell(fmt.Sprintf("%-20s", msg.Sender)).SetMaxWidth(21).SetTextColor(fg))
		l.Table.SetCell(idx, 3, tview.NewTableCell(formatters.FormatSize(msg.Size)).SetMaxWidth(5).SetTextColor(fg))
		l.Table.SetCell(idx, 4, tview.NewTableCell(msg.Subject).SetTextColor(fg))
	}

}

func (l *MessageList) UpdateMessage(idx int, msg *models.Message) *MessageList {
	if idx < 0 || idx >= len(l.messages) {
		return l
	}

	l.messages[idx] = msg
	l.updateCells(idx, idx+1)

	return l
}

func (l *MessageList) OnSelectionChanged(cb models.MessageCallback) *MessageList {
	l.onSelectionChanged = cb
	l.Table.SetSelectionChangedFunc(func(row int, column int) {
		cb(l.messages[row], row)
	})

	return l
}

func (l *MessageList) SelectedMessage() (*models.Message, int) {
	row, _ := l.Table.GetSelection()
	if row < 0 || row > len(l.messages) {
		return nil, -1
	}

	return l.messages[row], row
}
