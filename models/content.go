package models

type MessageContent struct {
	Mailer string
	Parts  []MessageContentPart
}

type MessageContentPart struct {
	ContentType string
	Content     []byte
}
