package models

type ActionType uint8

const (
	TypeDelete ActionType = iota
	TypeArchive
	TypeMarkAsRead
	TypeMoveToInboxUnread
	TypeMoveToInboxRead
	TypeMarkAsStarred
	TypeMarkAsUnstarred
)

type Action struct {
	Type    ActionType
	Message *Message
}
