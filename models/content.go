package models

type MessageContent struct {
	Parts []MessageContentPart
}

type MessageContentPart struct {
	ContentType string
	Content     []byte
}
