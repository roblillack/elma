package views

import (
	"fmt"

	"github.com/gdamore/tcell"
	"github.com/rivo/tview"
	"github.com/roblillack/mail/models"
	"github.com/roblillack/mail/views/formatters"
)

type MessageCallback func(msg *models.Message, idx int)

type MessageList struct {
	*tview.Table

	Messages         []*models.Message
	onDeleteMessage  MessageCallback
	onReplyToMessage MessageCallback
	onArchiveMessage MessageCallback
	onOpenMessage    MessageCallback
}

func NewMessageList() *MessageList {
	return &MessageList{
		Table: tview.NewTable().SetBorders(false).SetSelectable(true, false),
	}
}

func (l *MessageList) Select(index int) *MessageList {
	l.Table.Select(index, 0)
	return l
}

func (l *MessageList) SetMessages(messages []*models.Message) *MessageList {
	l.Messages = messages

	for idx, msg := range messages {
		fg := tview.Styles.PrimaryTextColor
		if msg.Status == models.StatusNew {
			fg = tcell.ColorRed
		}
		l.Table.SetCell(idx, 0, tview.NewTableCell(msg.FlagString()).SetMaxWidth(3).SetTextColor(fg))
		l.Table.SetCell(idx, 1, tview.NewTableCell(msg.Sent.Format("[Jan 02 15:04]")).SetTextColor(fg))
		l.Table.SetCell(idx, 2, tview.NewTableCell(fmt.Sprintf("%-20s", msg.Sender)).SetMaxWidth(21).SetTextColor(fg))
		l.Table.SetCell(idx, 3, tview.NewTableCell(formatters.FormatSize(msg.Size)).SetMaxWidth(5).SetTextColor(fg))
		l.Table.SetCell(idx, 4, tview.NewTableCell(msg.Subject).SetTextColor(fg))
	}

	return l
}

func (l *MessageList) OnDeleteMessage(cb MessageCallback) *MessageList {
	l.onDeleteMessage = cb
	return l
}

func (l *MessageList) OnReplyToMessage(cb MessageCallback) *MessageList {
	l.onReplyToMessage = cb
	return l
}

func (l *MessageList) OnArchiveMessage(cb MessageCallback) *MessageList {
	l.onArchiveMessage = cb
	return l
}

func (l *MessageList) OnOpenMessage(cb MessageCallback) *MessageList {
	l.onOpenMessage = cb
	return l
}

func (l *MessageList) OnSelectionChanged(cb MessageCallback) *MessageList {
	l.Table.SetSelectionChangedFunc(func(row int, column int) {
		cb(l.Messages[row], row)
	})
	return l
}

func (l *MessageList) Init() {
	l.Table = tview.NewTable().
		SetBorders(false).
		SetSelectable(true, false).
		SetSelectionChangedFunc(func(row int, column int) {

		}).
		Select(0, 0).
		SetDoneFunc(func(key tcell.Key) {
			if key == tcell.KeyEscape {
				//app.Stop()
			}
			//fmt.Println(key)
		}) /*.SetInputCapture(func(key *tcell.EventKey) *tcell.EventKey {
			return nil
		})*/
}
